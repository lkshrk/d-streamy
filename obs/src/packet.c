#include "packet.h"

#include <stdlib.h>
#include <string.h>

#define NAL_SPS 7
#define NAL_PPS 8
#define NAL_AUD 9

struct buffer {
	uint8_t *data;
	size_t size;
	size_t capacity;
};

struct ds_packet_converter {
	struct buffer sps;
	struct buffer pps;
	struct buffer out;
};

struct nal {
	size_t code_at;
	size_t payload_at;
	size_t end;
};

static const uint8_t start_code[4] = {0, 0, 0, 1};

static bool buffer_reserve(struct buffer *b, size_t capacity)
{
	if (capacity <= b->capacity)
		return true;
	uint8_t *grown = realloc(b->data, capacity);
	if (!grown)
		return false;
	b->data = grown;
	b->capacity = capacity;
	return true;
}

static bool buffer_append(struct buffer *b, const uint8_t *data, size_t size)
{
	if (size > SIZE_MAX - b->size || !buffer_reserve(b, b->size + size))
		return false;
	if (size)
		memcpy(b->data + b->size, data, size);
	b->size += size;
	return true;
}

static void buffer_free(struct buffer *b)
{
	free(b->data);
	b->data = NULL;
	b->size = b->capacity = 0;
}

static bool is_start_code(const uint8_t *data, size_t size, size_t at)
{
	return at + 3 <= size && data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1;
}

/* H.264 §B.1.1: a byte stream begins with an optional zero_byte and the start code 00 00 01. */
static bool starts_annex_b(const uint8_t *data, size_t size)
{
	return is_start_code(data, size, 0) || (size >= 4 && data[0] == 0 && is_start_code(data, size, 1));
}

static size_t find_start_code(const uint8_t *data, size_t size, size_t from)
{
	for (size_t i = from; i + 3 <= size; i++) {
		if (is_start_code(data, size, i))
			return i;
	}
	return size;
}

static bool next_nal(const uint8_t *data, size_t size, size_t from, struct nal *nal)
{
	size_t at = find_start_code(data, size, from);
	if (at == size)
		return false;
	size_t next = find_start_code(data, size, at + 3);
	size_t end = next;
	while (end > at + 3 && data[end - 1] == 0)
		end--;
	nal->code_at = (at > from && data[at - 1] == 0) ? at - 1 : at;
	nal->payload_at = at + 3;
	nal->end = end;
	return true;
}

static int nal_type(const uint8_t *data, const struct nal *nal)
{
	return nal->end > nal->payload_at ? data[nal->payload_at] & 0x1f : -1;
}

struct ds_packet_converter *ds_packet_converter_create(void)
{
	return calloc(1, sizeof(struct ds_packet_converter));
}

void ds_packet_converter_destroy(struct ds_packet_converter *conv)
{
	if (!conv)
		return;
	buffer_free(&conv->sps);
	buffer_free(&conv->pps);
	buffer_free(&conv->out);
	free(conv);
}

enum ds_packet_status ds_packet_set_video_extra_data(struct ds_packet_converter *conv, const uint8_t *data,
						     size_t size)
{
	if (!starts_annex_b(data, size))
		return DS_PACKET_AVCC_EXTRA_DATA;
	struct buffer sps = {0}, pps = {0};
	struct nal nal;
	for (size_t at = 0; next_nal(data, size, at, &nal); at = nal.payload_at) {
		int type = nal_type(data, &nal);
		struct buffer *set = type == NAL_SPS ? &sps : type == NAL_PPS ? &pps : NULL;
		if (set && !(buffer_append(set, start_code, sizeof(start_code)) &&
			     buffer_append(set, data + nal.payload_at, nal.end - nal.payload_at))) {
			buffer_free(&sps);
			buffer_free(&pps);
			return DS_PACKET_NO_MEMORY;
		}
	}
	if (!sps.size || !pps.size) {
		buffer_free(&sps);
		buffer_free(&pps);
		return DS_PACKET_EXTRA_DATA_INCOMPLETE;
	}
	buffer_free(&conv->sps);
	buffer_free(&conv->pps);
	conv->sps = sps;
	conv->pps = pps;
	return DS_PACKET_OK;
}

bool ds_packet_has_video_extra_data(const struct ds_packet_converter *conv)
{
	return conv->sps.size && conv->pps.size;
}

static enum ds_packet_status mul_add(int64_t a, int64_t b, int64_t c, int64_t *out)
{
	if (a != 0 && (a > INT64_MAX / b || a < INT64_MIN / b))
		return DS_PACKET_PTS_OVERFLOW;
	int64_t ab = a * b;
	if ((c > 0 && ab > INT64_MAX - c) || (c < 0 && ab < INT64_MIN - c))
		return DS_PACKET_PTS_OVERFLOW;
	*out = ab + c;
	return DS_PACKET_OK;
}

enum ds_packet_status ds_packet_pts_to_us(int64_t pts, int32_t timebase_num, int32_t timebase_den, int64_t *pts_us)
{
	if (timebase_num <= 0 || timebase_den <= 0)
		return DS_PACKET_BAD_TIMEBASE;
	const int64_t num = timebase_num, den = timebase_den, scale = num * 1000000;
	/* Split so no intermediate overflows; every part shares the sign of pts, so truncation composes. */
	int64_t q = pts / den, r = pts % den;
	int64_t t = r * num;
	int64_t tail = (t / den) * 1000000 + ((t % den) * 1000000) / den;
	return mul_add(q, scale, tail, pts_us);
}

enum ds_packet_status ds_packet_convert_audio(int64_t pts, int64_t dts, int32_t timebase_num, int32_t timebase_den,
					      int64_t *pts_us)
{
	if (pts != dts)
		return DS_PACKET_AUDIO_PTS_NE_DTS;
	return ds_packet_pts_to_us(pts, timebase_num, timebase_den, pts_us);
}

/* Offset after a leading AUD, which H.264 §7.4.1.2.3 requires first in the access unit; 0 otherwise. */
static size_t parameter_set_insert_at(const uint8_t *data, size_t size)
{
	struct nal first, second;
	if (!next_nal(data, size, 0, &first) || nal_type(data, &first) != NAL_AUD)
		return 0;
	return next_nal(data, size, first.payload_at, &second) ? second.code_at : size;
}

enum ds_packet_status ds_packet_convert_video(struct ds_packet_converter *conv, const uint8_t *data, size_t size,
					      bool keyframe, int64_t pts, int64_t dts, int32_t timebase_num,
					      int32_t timebase_den, struct ds_packet_frame *out)
{
	if (pts != dts)
		return DS_PACKET_B_FRAMES;
	if (!starts_annex_b(data, size))
		return DS_PACKET_NOT_ANNEX_B;
	int64_t pts_us;
	enum ds_packet_status status = ds_packet_pts_to_us(pts, timebase_num, timebase_den, &pts_us);
	if (status != DS_PACKET_OK)
		return status;

	bool has_sps = false, has_pps = false;
	if (keyframe) {
		struct nal nal;
		for (size_t at = 0; next_nal(data, size, at, &nal); at = nal.payload_at) {
			int type = nal_type(data, &nal);
			has_sps |= type == NAL_SPS;
			has_pps |= type == NAL_PPS;
		}
	}

	out->pts_us = pts_us;
	out->keyframe = keyframe;
	if (!keyframe || (has_sps && has_pps)) {
		out->data = data;
		out->size = size;
		return DS_PACKET_OK;
	}
	if ((!has_sps && !conv->sps.size) || (!has_pps && !conv->pps.size))
		return DS_PACKET_NO_PARAMETER_SETS;

	size_t at = parameter_set_insert_at(data, size);
	conv->out.size = 0;
	bool ok = buffer_append(&conv->out, data, at) &&
		  (has_sps || buffer_append(&conv->out, conv->sps.data, conv->sps.size)) &&
		  (has_pps || buffer_append(&conv->out, conv->pps.data, conv->pps.size)) &&
		  buffer_append(&conv->out, data + at, size - at);
	if (!ok)
		return DS_PACKET_NO_MEMORY;
	out->data = conv->out.data;
	out->size = conv->out.size;
	return DS_PACKET_OK;
}

const char *ds_packet_status_message(enum ds_packet_status status)
{
	switch (status) {
	case DS_PACKET_OK:
		return "ok";
	case DS_PACKET_AVCC_EXTRA_DATA:
		return "the video encoder's extra data is avcC, not Annex B";
	case DS_PACKET_NOT_ANNEX_B:
		return "video packet is not Annex B: it does not start with a start code";
	case DS_PACKET_NO_PARAMETER_SETS:
		return "keyframe has no SPS/PPS and the encoder has no extra data yet";
	case DS_PACKET_EXTRA_DATA_INCOMPLETE:
		return "the video encoder's extra data lacks an SPS or a PPS";
	case DS_PACKET_B_FRAMES:
		return "B-frames are enabled: video pts differs from dts";
	case DS_PACKET_AUDIO_PTS_NE_DTS:
		return "audio pts differs from dts";
	case DS_PACKET_BAD_TIMEBASE:
		return "packet timebase is not positive";
	case DS_PACKET_PTS_OVERFLOW:
		return "packet pts does not fit in microseconds";
	case DS_PACKET_NO_MEMORY:
		return "out of memory";
	}
	return "unknown packet status";
}
