#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "packet.h"

#define TRACE_HEADER 40
#define NAL_IDR 5
#define NAL_SEI 6
#define NAL_SPS 7
#define NAL_PPS 8
#define NAL_AUD 9
#define MAX_NALS 64

static int failures;

#define CHECK(cond, ...)                                                       \
	do {                                                                   \
		if (!(cond)) {                                                 \
			failures++;                                            \
			fprintf(stderr, "%s:%d: %s: ", __FILE__, __LINE__, #cond); \
			fprintf(stderr, __VA_ARGS__);                          \
			fputc('\n', stderr);                                   \
		}                                                              \
	} while (0)

struct bytes {
	uint8_t *data;
	size_t size;
};

struct nal_list {
	int count;
	int type[MAX_NALS];
	size_t start_code_len[MAX_NALS];
	const uint8_t *payload[MAX_NALS];
	size_t len[MAX_NALS];
};

static bool read_file(const char *dir, const char *name, struct bytes *out)
{
	char path[4096];
	snprintf(path, sizeof(path), "%s/%s", dir, name);
	FILE *f = fopen(path, "rb");
	if (!f)
		return false;
	out->data = NULL;
	out->size = 0;
	size_t capacity = 0;
	for (;;) {
		if (out->size == capacity) {
			capacity = capacity ? capacity * 2 : 1 << 16;
			uint8_t *grown = realloc(out->data, capacity);
			if (!grown) {
				fclose(f);
				return false;
			}
			out->data = grown;
		}
		size_t n = fread(out->data + out->size, 1, capacity - out->size, f);
		if (n == 0)
			break;
		out->size += n;
	}
	fclose(f);
	return true;
}

static uint32_t u32_le(const uint8_t *p)
{
	return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

static int64_t i64_le(const uint8_t *p)
{
	uint64_t v = 0;
	for (int i = 7; i >= 0; i--)
		v = v << 8 | p[i];
	return (int64_t)v;
}

/* Independent of packet.c: start codes 00 00 01 with an optional zero_byte, trailing zeros dropped (H.264 §B.1). */
static void parse_nals(const uint8_t *data, size_t size, struct nal_list *list)
{
	list->count = 0;
	size_t i = 0;
	while (i + 3 <= size && list->count < MAX_NALS) {
		if (!(data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)) {
			i++;
			continue;
		}
		int n = list->count++;
		list->start_code_len[n] = (i > 0 && data[i - 1] == 0) ? 4 : 3;
		list->payload[n] = data + i + 3;
		size_t j = i + 3;
		while (j + 3 <= size && !(data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1))
			j++;
		size_t end = j + 3 <= size ? j : size;
		while (end > i + 3 && data[end - 1] == 0)
			end--;
		list->len[n] = end - (i + 3);
		list->type[n] = list->len[n] ? list->payload[n][0] & 0x1f : -1;
		i = j;
	}
}

static int count_type(const struct nal_list *list, int type)
{
	int n = 0;
	for (int i = 0; i < list->count; i++)
		n += list->type[i] == type;
	return n;
}

static bool same_nal(const struct nal_list *a, int i, const struct nal_list *b, int j)
{
	return a->len[i] == b->len[j] && memcmp(a->payload[i], b->payload[j], a->len[i]) == 0;
}

static bool keeps_other_nals(const struct nal_list *in, const struct nal_list *out)
{
	int j = 0;
	for (int i = 0; i < in->count; i++) {
		if (in->type[i] == NAL_SPS || in->type[i] == NAL_PPS)
			continue;
		while (j < out->count && (out->type[j] == NAL_SPS || out->type[j] == NAL_PPS))
			j++;
		if (j >= out->count || !same_nal(in, i, out, j))
			return false;
		j++;
	}
	return true;
}

static size_t strip_parameter_sets(const uint8_t *data, size_t size, uint8_t *out)
{
	struct nal_list list;
	parse_nals(data, size, &list);
	size_t n = 0;
	for (int i = 0; i < list.count; i++) {
		if (list.type[i] == NAL_SPS || list.type[i] == NAL_PPS)
			continue;
		static const uint8_t code[4] = {0, 0, 0, 1};
		size_t k = list.start_code_len[i];
		memcpy(out + n, code + 4 - k, k);
		n += k;
		memcpy(out + n, list.payload[i], list.len[i]);
		n += list.len[i];
	}
	return n;
}

static int64_t naive_us(int64_t pts, int32_t num, int32_t den)
{
	return pts * 1000000 * num / den;
}

static bool is_b_frame_trace(const char *dir)
{
	return strstr(dir, "-bf2-") || strstr(dir, "-bfunset-");
}

static void check_trace(const char *dir)
{
	struct bytes packets, extra;
	if (!read_file(dir, "packets.bin", &packets) || !read_file(dir, "video-extra.bin", &extra)) {
		CHECK(false, "%s: cannot read packets.bin or video-extra.bin", dir);
		return;
	}
	CHECK(packets.size >= 8 && memcmp(packets.data, "DSTRACE1", 8) == 0, "%s: magic", dir);

	struct ds_packet_converter *conv = ds_packet_converter_create();
	struct ds_packet_converter *stripped = ds_packet_converter_create();
	CHECK(ds_packet_set_video_extra_data(stripped, extra.data, extra.size) == DS_PACKET_OK, "%s", dir);
	struct nal_list extra_nals;
	parse_nals(extra.data, extra.size, &extra_nals);

	uint8_t *scratch = malloc(packets.size);
	bool b_frames = is_b_frame_trace(dir);
	enum ds_packet_status first_refusal = DS_PACKET_OK;
	int video = 0, audio = 0, keyframes = 0;
	int64_t last_video_us = INT64_MIN;
	for (size_t at = 8; at + TRACE_HEADER <= packets.size;) {
		const uint8_t *h = packets.data + at;
		bool is_video = h[0] == 1, keyframe = h[1] == 1;
		int32_t num = (int32_t)u32_le(h + 4), den = (int32_t)u32_le(h + 8);
		size_t size = u32_le(h + 12);
		int64_t pts = i64_le(h + 16), dts = i64_le(h + 24);
		const uint8_t *data = h + TRACE_HEADER;
		at += TRACE_HEADER + size;
		CHECK(at <= packets.size, "%s: truncated", dir);
		if (at > packets.size)
			break;

		if (!is_video) {
			int64_t us = 0;
			CHECK(ds_packet_convert_audio(pts, dts, num, den, &us) == DS_PACKET_OK, "%s audio %d", dir, audio);
			CHECK(us == naive_us(pts, num, den), "%s audio %d: %" PRId64, dir, audio, us);
			if (audio == 0 && strstr(dir, "opus"))
				CHECK(us == -6500, "%s: first Opus pts %" PRId64 " µs", dir, us);
			audio++;
			continue;
		}

		if (video == 0)
			CHECK(!ds_packet_has_video_extra_data(conv), "%s: extra data before the first packet", dir);
		struct ds_packet_frame out;
		enum ds_packet_status s =
			ds_packet_convert_video(conv, data, size, keyframe, pts, dts, num, den, &out);
		if (video == 0)
			CHECK(ds_packet_set_video_extra_data(conv, extra.data, extra.size) == DS_PACKET_OK, "%s", dir);
		video++;
		if (s != DS_PACKET_OK) {
			if (first_refusal == DS_PACKET_OK)
				first_refusal = s;
			continue;
		}
		CHECK(out.pts_us == naive_us(pts, num, den), "%s video %d", dir, video);
		CHECK(out.pts_us > last_video_us, "%s video %d: pts not increasing", dir, video);
		last_video_us = out.pts_us;
		CHECK(out.keyframe == keyframe, "%s video %d", dir, video);
		CHECK(out.data == data && out.size == size, "%s video %d: not passed through", dir, video);

		if (!keyframe)
			continue;
		keyframes++;
		size_t n = strip_parameter_sets(data, size, scratch);
		struct ds_packet_frame rebuilt;
		CHECK(ds_packet_convert_video(stripped, scratch, n, true, pts, dts, num, den, &rebuilt) ==
			      DS_PACKET_OK,
		      "%s video %d: stripped keyframe", dir, video);
		struct nal_list in_nals, out_nals;
		parse_nals(data, size, &in_nals);
		parse_nals(rebuilt.data, rebuilt.size, &out_nals);
		CHECK(count_type(&out_nals, NAL_SPS) == 1 && count_type(&out_nals, NAL_PPS) == 1,
		      "%s video %d: SPS/PPS not prepended exactly once", dir, video);
		CHECK(out_nals.count >= 2 && same_nal(&out_nals, 0, &extra_nals, 0) &&
			      same_nal(&out_nals, 1, &extra_nals, 1),
		      "%s video %d: prepended sets are not the extra data's", dir, video);
		CHECK(keeps_other_nals(&in_nals, &out_nals), "%s video %d: other NAL units changed", dir, video);
	}
	CHECK(video > 0, "%s: no video", dir);
	if (b_frames) {
		CHECK(first_refusal == DS_PACKET_B_FRAMES, "%s: refused with %d", dir, first_refusal);
		CHECK(strstr(ds_packet_status_message(first_refusal), "B-frames are enabled") != NULL, "%s", dir);
	} else {
		CHECK(first_refusal == DS_PACKET_OK, "%s: refused: %s", dir, ds_packet_status_message(first_refusal));
		CHECK(keyframes > 0, "%s: no keyframes", dir);
	}
	printf("trace %s: %d video, %d audio, %d keyframes\n", dir, video, audio, keyframes);

	free(scratch);
	ds_packet_converter_destroy(conv);
	ds_packet_converter_destroy(stripped);
	free(packets.data);
	free(extra.data);
}

static const uint8_t SPS[] = {0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01};
static const uint8_t PPS[] = {0x68, 0xce, 0x3c, 0x80};
static const uint8_t AUD[] = {0x09, 0xf0};
static const uint8_t SEI[] = {0x06, 0x05, 0x01, 0xaa, 0x80};
static const uint8_t IDR_A[] = {0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0x21};
static const uint8_t IDR_B[] = {0x65, 0x00, 0xa0, 0x00, 0x00, 0x03, 0x00, 0x7f};
static const uint8_t SLICE[] = {0x41, 0x9a, 0x00, 0x00, 0x03, 0x02, 0x10};

struct au {
	uint8_t data[512];
	size_t size;
};

static void put(struct au *au, int start_code_len, const uint8_t *nal, size_t len)
{
	static const uint8_t code[4] = {0, 0, 0, 1};
	memcpy(au->data + au->size, code + 4 - start_code_len, (size_t)start_code_len);
	au->size += (size_t)start_code_len;
	memcpy(au->data + au->size, nal, len);
	au->size += len;
}

#define PUT(au, sc, nal) put(au, sc, nal, sizeof(nal))

static struct ds_packet_converter *with_extra(int start_code_len)
{
	struct ds_packet_converter *conv = ds_packet_converter_create();
	struct au extra = {.size = 0};
	PUT(&extra, start_code_len, SPS);
	PUT(&extra, start_code_len, PPS);
	CHECK(ds_packet_set_video_extra_data(conv, extra.data, extra.size) == DS_PACKET_OK, "extra data");
	return conv;
}

static void expect_types(const struct ds_packet_frame *out, const int *types, int n, const char *what)
{
	struct nal_list list;
	parse_nals(out->data, out->size, &list);
	bool same = list.count == n;
	for (int i = 0; same && i < n; i++)
		same = list.type[i] == types[i];
	CHECK(same, "%s: %d NAL units, first type %d", what, list.count, list.count ? list.type[0] : -1);
}

static bool contains(const struct ds_packet_frame *out, const uint8_t *nal, size_t len)
{
	for (size_t i = 0; i + len <= out->size; i++) {
		if (memcmp(out->data + i, nal, len) == 0)
			return true;
	}
	return false;
}

static enum ds_packet_status key(struct ds_packet_converter *conv, const struct au *au, struct ds_packet_frame *out)
{
	return ds_packet_convert_video(conv, au->data, au->size, true, 0, 0, 1, 30, out);
}

static void test_start_codes(void)
{
	for (int sc = 3; sc <= 4; sc++) {
		struct ds_packet_converter *conv = with_extra(sc);
		struct au au = {.size = 0};
		PUT(&au, sc, IDR_A);
		struct ds_packet_frame out;
		CHECK(key(conv, &au, &out) == DS_PACKET_OK, "%d-byte IDR", sc);
		expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_IDR}, 3, "prepended");
		CHECK(out.size == 4 + sizeof(SPS) + 4 + sizeof(PPS) + au.size, "%d-byte: size %zu", sc, out.size);
		CHECK(memcmp(out.data + out.size - au.size, au.data, au.size) == 0, "%d-byte: IDR not kept", sc);

		struct au p = {.size = 0};
		PUT(&p, sc, SLICE);
		CHECK(ds_packet_convert_video(conv, p.data, p.size, false, 1, 1, 1, 30, &out) == DS_PACKET_OK, "P");
		CHECK(out.data == p.data && out.size == p.size, "%d-byte P frame not passed through", sc);
		ds_packet_converter_destroy(conv);
	}
}

static void test_aud_and_sei(void)
{
	struct ds_packet_converter *conv = with_extra(4);
	struct au au = {.size = 0};
	PUT(&au, 4, AUD);
	PUT(&au, 3, SEI);
	PUT(&au, 3, IDR_A);
	struct ds_packet_frame out;
	CHECK(key(conv, &au, &out) == DS_PACKET_OK, "AUD SEI IDR");
	expect_types(&out, (int[]){NAL_AUD, NAL_SPS, NAL_PPS, NAL_SEI, NAL_IDR}, 5, "AUD first, then SPS/PPS");
	CHECK(contains(&out, SEI, sizeof(SEI)) && contains(&out, IDR_A, sizeof(IDR_A)), "SEI/IDR changed");

	struct au sei_first = {.size = 0};
	PUT(&sei_first, 4, SEI);
	PUT(&sei_first, 4, IDR_A);
	CHECK(key(conv, &sei_first, &out) == DS_PACKET_OK, "SEI IDR");
	expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_SEI, NAL_IDR}, 4, "SPS/PPS before SEI");

	struct au aud_only = {.size = 0};
	PUT(&aud_only, 4, AUD);
	PUT(&aud_only, 4, SPS);
	PUT(&aud_only, 4, PPS);
	PUT(&aud_only, 4, IDR_A);
	CHECK(key(conv, &aud_only, &out) == DS_PACKET_OK && out.data == aud_only.data, "in-band sets kept as is");
	ds_packet_converter_destroy(conv);
}

static void test_multiple_slices(void)
{
	struct ds_packet_converter *conv = with_extra(4);
	struct au au = {.size = 0};
	PUT(&au, 4, IDR_A);
	PUT(&au, 3, IDR_B);
	PUT(&au, 3, IDR_A);
	struct ds_packet_frame out;
	CHECK(key(conv, &au, &out) == DS_PACKET_OK, "three slices");
	expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_IDR, NAL_IDR, NAL_IDR}, 5, "sets once before all slices");
	CHECK(memcmp(out.data + out.size - au.size, au.data, au.size) == 0, "slices not kept");

	struct au p = {.size = 0};
	PUT(&p, 4, SLICE);
	PUT(&p, 3, SLICE);
	CHECK(ds_packet_convert_video(conv, p.data, p.size, false, 1, 1, 1, 30, &out) == DS_PACKET_OK &&
		      out.data == p.data,
	      "multi-slice P frame");
	ds_packet_converter_destroy(conv);
}

static void test_emulation_prevention_untouched(void)
{
	struct ds_packet_converter *conv = with_extra(4);
	struct au au = {.size = 0};
	PUT(&au, 4, IDR_A);
	PUT(&au, 4, IDR_B);
	struct ds_packet_frame out;
	CHECK(key(conv, &au, &out) == DS_PACKET_OK, "IDR with 00 00 03");
	CHECK(memcmp(out.data + out.size - au.size, au.data, au.size) == 0, "emulation prevention bytes changed");
	CHECK(contains(&out, (const uint8_t[]){0x00, 0x00, 0x03, 0x01}, 4), "00 00 03 01 lost");
	CHECK(contains(&out, (const uint8_t[]){0x00, 0x00, 0x03, 0x00}, 4), "00 00 03 00 lost");
	ds_packet_converter_destroy(conv);
}

static void test_never_duplicates(void)
{
	struct ds_packet_converter *conv = with_extra(4);
	struct ds_packet_frame out;
	struct au sps_only = {.size = 0};
	PUT(&sps_only, 4, SPS);
	PUT(&sps_only, 4, IDR_A);
	CHECK(key(conv, &sps_only, &out) == DS_PACKET_OK, "SPS in band");
	expect_types(&out, (int[]){NAL_PPS, NAL_SPS, NAL_IDR}, 3, "only the missing PPS");

	struct au pps_only = {.size = 0};
	PUT(&pps_only, 4, PPS);
	PUT(&pps_only, 4, IDR_A);
	CHECK(key(conv, &pps_only, &out) == DS_PACKET_OK, "PPS in band");
	expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_IDR}, 3, "only the missing SPS");

	struct au both = {.size = 0};
	PUT(&both, 4, SPS);
	PUT(&both, 4, PPS);
	PUT(&both, 4, IDR_A);
	CHECK(key(conv, &both, &out) == DS_PACKET_OK && out.data == both.data && out.size == both.size,
	      "in-band SPS/PPS passed through");

	for (int i = 0; i < 3; i++) {
		struct au idr = {.size = 0};
		PUT(&idr, 4, IDR_A);
		CHECK(key(conv, &idr, &out) == DS_PACKET_OK, "keyframe %d", i);
		expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_IDR}, 3, "every keyframe once");
	}
	ds_packet_converter_destroy(conv);
}

static void test_lazy_extra_data(void)
{
	struct ds_packet_converter *conv = ds_packet_converter_create();
	struct ds_packet_frame out;
	struct au idr = {.size = 0};
	PUT(&idr, 4, IDR_A);
	CHECK(!ds_packet_has_video_extra_data(conv), "fresh converter");
	CHECK(key(conv, &idr, &out) == DS_PACKET_NO_PARAMETER_SETS, "keyframe without any SPS/PPS");

	struct au inband = {.size = 0};
	PUT(&inband, 4, SPS);
	PUT(&inband, 4, PPS);
	PUT(&inband, 4, IDR_A);
	CHECK(key(conv, &inband, &out) == DS_PACKET_OK && out.data == inband.data, "in band before extra data");

	struct au extra = {.size = 0};
	PUT(&extra, 4, SPS);
	PUT(&extra, 4, PPS);
	CHECK(ds_packet_set_video_extra_data(conv, extra.data, extra.size) == DS_PACKET_OK, "late extra data");
	CHECK(ds_packet_has_video_extra_data(conv), "after extra data");
	CHECK(key(conv, &idr, &out) == DS_PACKET_OK, "keyframe after late extra data");
	expect_types(&out, (int[]){NAL_SPS, NAL_PPS, NAL_IDR}, 3, "late extra data prepended");

	struct au sps_only = {.size = 0};
	PUT(&sps_only, 4, SPS);
	CHECK(ds_packet_set_video_extra_data(conv, sps_only.data, sps_only.size) == DS_PACKET_EXTRA_DATA_INCOMPLETE,
	      "extra data without a PPS");
	CHECK(ds_packet_has_video_extra_data(conv), "incomplete extra data keeps the old sets");
	ds_packet_converter_destroy(conv);
}

static void test_avcc_rejected(void)
{
	/* avcC: version 1, profile, compat, level, 0xff (4-byte lengths), 0xe1 (one SPS), u16 length, SPS... */
	const uint8_t avcc[] = {0x01, 0x42, 0xc0, 0x1f, 0xff, 0xe1, 0x00, 0x06, 0x67, 0x42, 0xc0, 0x1f, 0xda,
				0x01, 0x01, 0x00, 0x04, 0x68, 0xce, 0x3c, 0x80};
	struct ds_packet_converter *conv = ds_packet_converter_create();
	CHECK(ds_packet_set_video_extra_data(conv, avcc, sizeof(avcc)) == DS_PACKET_AVCC_EXTRA_DATA, "avcC");
	CHECK(!ds_packet_has_video_extra_data(conv), "avcC not taken");
	CHECK(ds_packet_set_video_extra_data(conv, avcc, 0) == DS_PACKET_AVCC_EXTRA_DATA, "empty extra data");

	const uint8_t length_prefixed[] = {0x00, 0x00, 0x00, 0x08, 0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0x21};
	struct ds_packet_frame out;
	CHECK(ds_packet_convert_video(conv, length_prefixed, sizeof(length_prefixed), true, 0, 0, 1, 30, &out) ==
		      DS_PACKET_NOT_ANNEX_B,
	      "AVCC packet");
	CHECK(ds_packet_convert_video(conv, length_prefixed, 3, false, 0, 0, 1, 30, &out) == DS_PACKET_NOT_ANNEX_B,
	      "short packet");
	ds_packet_converter_destroy(conv);
}

static void test_pts_conversion(void)
{
	struct {
		int64_t pts;
		int32_t num, den;
		int64_t us;
	} cases[] = {
		{0, 1, 30, 0},           {1, 1, 30, 33333},          {149, 1, 30, 4966666},     {1, 1, 60, 16666},
		{299, 1, 60, 4983333},   {1, 1, 15, 66666},          {-312, 1, 48000, -6500},   {648, 1, 48000, 13500},
		{960, 1, 48000, 20000},  {-1024, 1, 48000, -21333},  {-1, 1, 30, -33333},       {-2, 1, 30, -66666},
		{1001, 1001, 30000, 33400033}, {7, 1, 1000000, 7},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		int64_t us = 0;
		CHECK(ds_packet_pts_to_us(cases[i].pts, cases[i].num, cases[i].den, &us) == DS_PACKET_OK, "case %zu", i);
		CHECK(us == cases[i].us, "case %zu: %" PRId64 " -> %" PRId64 ", want %" PRId64, i, cases[i].pts, us,
		      cases[i].us);
	}
	for (int64_t pts = -100000; pts <= 100000; pts += 7) {
		int64_t a = 0, b = 0;
		ds_packet_pts_to_us(pts, 1, 30, &a);
		ds_packet_pts_to_us(pts, 1, 48000, &b);
		CHECK(a == naive_us(pts, 1, 30) && b == naive_us(pts, 1, 48000), "pts %" PRId64, pts);
	}
	int64_t us = 0;
	CHECK(ds_packet_pts_to_us(INT64_C(30000000000001), 1, 30, &us) == DS_PACKET_OK &&
		      us == INT64_C(1000000000000033333),
	      "pts whose naive product overflows");
	CHECK(ds_packet_pts_to_us(INT64_MAX, 1, 1, &us) == DS_PACKET_PTS_OVERFLOW, "overflow");
	CHECK(ds_packet_pts_to_us(INT64_MIN, 1, 1, &us) == DS_PACKET_PTS_OVERFLOW, "negative overflow");
	CHECK(ds_packet_pts_to_us(1, 0, 30, &us) == DS_PACKET_BAD_TIMEBASE, "zero numerator");
	CHECK(ds_packet_pts_to_us(1, 1, 0, &us) == DS_PACKET_BAD_TIMEBASE, "zero denominator");
	CHECK(ds_packet_pts_to_us(1, 1, -30, &us) == DS_PACKET_BAD_TIMEBASE, "negative denominator");

	CHECK(ds_packet_convert_audio(-312, -312, 1, 48000, &us) == DS_PACKET_OK && us == -6500, "Opus priming");
	CHECK(ds_packet_convert_audio(0, 1, 1, 48000, &us) == DS_PACKET_AUDIO_PTS_NE_DTS, "audio pts != dts");
}

static void test_b_frames_refused(void)
{
	struct ds_packet_converter *conv = with_extra(4);
	struct au idr = {.size = 0};
	PUT(&idr, 4, SPS);
	PUT(&idr, 4, PPS);
	PUT(&idr, 4, IDR_A);
	struct ds_packet_frame out;
	CHECK(ds_packet_convert_video(conv, idr.data, idr.size, true, 0, -2, 1, 30, &out) == DS_PACKET_B_FRAMES,
	      "pts > dts");
	CHECK(ds_packet_convert_video(conv, idr.data, idr.size, true, 1, 2, 1, 30, &out) == DS_PACKET_B_FRAMES,
	      "pts < dts");
	CHECK(strncmp(ds_packet_status_message(DS_PACKET_B_FRAMES), "B-frames are enabled", 20) == 0, "message");
	CHECK(ds_packet_convert_video(conv, idr.data, idr.size, true, 3, 3, 1, 30, &out) == DS_PACKET_OK, "pts == dts");
	ds_packet_converter_destroy(conv);
}

int main(int argc, char **argv)
{
	test_start_codes();
	test_aud_and_sei();
	test_multiple_slices();
	test_emulation_prevention_untouched();
	test_never_duplicates();
	test_lazy_extra_data();
	test_avcc_rejected();
	test_pts_conversion();
	test_b_frames_refused();

	int traces = 0;
	for (int i = 1; i < argc; i++) {
		check_trace(argv[i]);
		traces++;
	}
	CHECK(traces >= 9, "only %d traces given", traces);
	if (failures) {
		fprintf(stderr, "packet_test: %d failures\n", failures);
		return 1;
	}
	printf("packet_test: PASS (%d traces)\n", traces);
	return 0;
}
