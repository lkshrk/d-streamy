#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum ds_packet_status {
	DS_PACKET_OK = 0,
	DS_PACKET_AVCC_EXTRA_DATA = 1,
	DS_PACKET_NOT_ANNEX_B = 2,
	DS_PACKET_NO_PARAMETER_SETS = 3,
	DS_PACKET_EXTRA_DATA_INCOMPLETE = 4,
	DS_PACKET_B_FRAMES = 5,
	DS_PACKET_AUDIO_PTS_NE_DTS = 6,
	DS_PACKET_BAD_TIMEBASE = 7,
	DS_PACKET_PTS_OVERFLOW = 8,
	DS_PACKET_NO_MEMORY = 9,
};

struct ds_packet_converter;

struct ds_packet_frame {
	/* Points into the input packet or the converter; valid until the next conversion or destroy. */
	const uint8_t *data;
	size_t size;
	int64_t pts_us;
	bool keyframe;
};

struct ds_packet_converter *ds_packet_converter_create(void);
void ds_packet_converter_destroy(struct ds_packet_converter *conv);

/* Takes the SPS and PPS from Annex B extra data; may be called at any time, e.g. once the encoder exposes it. */
enum ds_packet_status ds_packet_set_video_extra_data(struct ds_packet_converter *conv, const uint8_t *data,
						     size_t size);
bool ds_packet_has_video_extra_data(const struct ds_packet_converter *conv);

enum ds_packet_status ds_packet_convert_video(struct ds_packet_converter *conv, const uint8_t *data, size_t size,
					      bool keyframe, int64_t pts, int64_t dts, int32_t timebase_num,
					      int32_t timebase_den, struct ds_packet_frame *out);

enum ds_packet_status ds_packet_convert_audio(int64_t pts, int64_t dts, int32_t timebase_num, int32_t timebase_den,
					      int64_t *pts_us);

/* Truncates toward zero, like the Rust side's `pts * 1_000_000 * num / den`. */
enum ds_packet_status ds_packet_pts_to_us(int64_t pts, int32_t timebase_num, int32_t timebase_den, int64_t *pts_us);

const char *ds_packet_status_message(enum ds_packet_status status);

#ifdef __cplusplus
}
#endif
