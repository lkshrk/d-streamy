#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <obs.h>
#include <util/base.h>
#include <util/bmem.h>
#include <util/platform.h>
#include <util/threading.h>

#define SHOWN_PACKETS 4

struct spike_args {
	const char *graphics;
	const char *video_encoder;
	const char *audio_encoder;
	const char *plugin_bin;
	const char *plugin_data;
	const char *modules;
	int seconds;
};

struct spike_output {
	obs_output_t *output;
	pthread_t stop_thread;
	bool stop_thread_active;
};

static volatile long video_packets;
static volatile long audio_packets;
static volatile long video_keyframes;
static volatile long video_pts_ne_dts;
static volatile long audio_first_frames_logged;
static volatile long video_first_frames_logged;
static os_event_t *stopped_event;

static void log_handler(int lvl, const char *msg, va_list args, void *param)
{
	UNUSED_PARAMETER(param);
	if (lvl > LOG_INFO)
		return;
	const char *tag = lvl <= LOG_ERROR ? "error" : lvl == LOG_WARNING ? "warn" : "info";
	char buf[4096];
	vsnprintf(buf, sizeof(buf), msg, args);
	printf("[obs %s] %s\n", tag, buf);
	fflush(stdout);
}

static void hex(const char *label, const uint8_t *data, size_t size, size_t max)
{
	printf("SPIKE %s (%zu bytes):", label, size);
	for (size_t i = 0; i < size && i < max; i++)
		printf(" %02x", data[i]);
	printf("%s\n", size > max ? " ..." : "");
}

static size_t annexb_start_code(const uint8_t *d, size_t size, size_t i)
{
	if (i + 3 <= size && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)
		return 3;
	if (i + 4 <= size && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1)
		return 4;
	return 0;
}

static void describe_h264(const char *label, const uint8_t *d, size_t size)
{
	if (size >= 1 && d[0] == 1 && size >= 7) {
		printf("SPIKE %s format=avcC profile=%u level=%u nal_length_size=%u\n", label, d[1], d[3],
		       (d[4] & 3) + 1);
		return;
	}
	if (annexb_start_code(d, size, 0)) {
		printf("SPIKE %s format=annexb nal_types=", label);
		for (size_t i = 0; i < size;) {
			size_t sc = annexb_start_code(d, size, i);
			if (sc && i + sc < size) {
				printf("%u,", d[i + sc] & 0x1f);
				i += sc + 1;
			} else {
				i++;
			}
		}
		printf("\n");
		return;
	}
	printf("SPIKE %s format=length-prefixed? nal_types=", label);
	for (size_t i = 0; i + 4 < size;) {
		uint32_t len = ((uint32_t)d[i] << 24) | ((uint32_t)d[i + 1] << 16) | ((uint32_t)d[i + 2] << 8) | d[i + 3];
		if (len == 0 || i + 4 + len > size) {
			printf("(bad length %" PRIu32 " at %zu)", len, i);
			break;
		}
		printf("%u,", d[i + 4] & 0x1f);
		i += 4 + len;
	}
	printf("\n");
}

static void describe_opus(const uint8_t *d, size_t size)
{
	if (!size)
		return;
	unsigned config = d[0] >> 3;
	static const double silk[] = {10, 20, 40, 60};
	static const double celt[] = {2.5, 5, 10, 20};
	double ms = config < 12 ? silk[config & 3] : config < 16 ? ((config & 1) ? 20 : 10) : celt[config & 3];
	const char *mode = config < 12 ? "silk" : config < 16 ? "hybrid" : "celt";
	printf("SPIKE opus toc=0x%02x config=%u mode=%s frame_ms=%.1f stereo=%u code=%u\n", d[0], config, mode, ms,
	       (d[0] >> 2) & 1, d[0] & 3);
}

static const char *spike_output_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return "Spike Output";
}

static void *spike_output_create(obs_data_t *settings, obs_output_t *output)
{
	UNUSED_PARAMETER(settings);
	struct spike_output *ctx = bzalloc(sizeof(*ctx));
	ctx->output = output;
	return ctx;
}

static void spike_output_destroy(void *data)
{
	struct spike_output *ctx = data;
	if (ctx->stop_thread_active)
		pthread_join(ctx->stop_thread, NULL);
	bfree(ctx);
}

static bool spike_output_start(void *data)
{
	struct spike_output *ctx = data;
	if (!obs_output_can_begin_data_capture(ctx->output, 0))
		return false;
	if (!obs_output_initialize_encoders(ctx->output, 0))
		return false;
	if (ctx->stop_thread_active)
		pthread_join(ctx->stop_thread, NULL);
	return obs_output_begin_data_capture(ctx->output, 0);
}

static void *spike_stop_thread(void *data)
{
	struct spike_output *ctx = data;
	obs_output_end_data_capture(ctx->output);
	ctx->stop_thread_active = false;
	return NULL;
}

static void spike_output_stop(void *data, uint64_t ts)
{
	UNUSED_PARAMETER(ts);
	struct spike_output *ctx = data;
	ctx->stop_thread_active = pthread_create(&ctx->stop_thread, NULL, spike_stop_thread, data) == 0;
}

static void spike_output_packet(void *data, struct encoder_packet *pkt)
{
	UNUSED_PARAMETER(data);
	if (!pkt)
		return;

	if (pkt->type == OBS_ENCODER_VIDEO) {
		long n = os_atomic_inc_long(&video_packets);
		if (pkt->keyframe)
			os_atomic_inc_long(&video_keyframes);
		if (pkt->pts != pkt->dts)
			os_atomic_inc_long(&video_pts_ne_dts);
		if (n <= SHOWN_PACKETS || (pkt->keyframe && os_atomic_inc_long(&video_first_frames_logged) <= 2)) {
			printf("SPIKE video packet #%ld size=%zu keyframe=%d pts=%" PRId64 " dts=%" PRId64
			       " timebase=%d/%d priority=%d\n",
			       n, pkt->size, pkt->keyframe, pkt->pts, pkt->dts, pkt->timebase_num, pkt->timebase_den,
			       pkt->priority);
			hex("video packet bytes", pkt->data, pkt->size, 48);
			describe_h264("video packet", pkt->data, pkt->size);
		}
	} else {
		long n = os_atomic_inc_long(&audio_packets);
		if (n <= SHOWN_PACKETS || (n % 50 == 0 && os_atomic_inc_long(&audio_first_frames_logged) <= 2)) {
			printf("SPIKE audio packet #%ld size=%zu pts=%" PRId64 " dts=%" PRId64 " timebase=%d/%d\n", n,
			       pkt->size, pkt->pts, pkt->dts, pkt->timebase_num, pkt->timebase_den);
			hex("audio packet bytes", pkt->data, pkt->size, 16);
			describe_opus(pkt->data, pkt->size);
		}
	}
	fflush(stdout);
}

static struct obs_output_info spike_output_info = {
	.id = "dstreamy_spike_output",
	.flags = OBS_OUTPUT_AV | OBS_OUTPUT_ENCODED,
	.get_name = spike_output_name,
	.create = spike_output_create,
	.destroy = spike_output_destroy,
	.start = spike_output_start,
	.stop = spike_output_stop,
	.encoded_packet = spike_output_packet,
	.encoded_video_codecs = "h264",
	.encoded_audio_codecs = "opus",
};

static void on_output_stop(void *param, calldata_t *cd)
{
	UNUSED_PARAMETER(param);
	printf("SPIKE output stop signal code=%lld\n", (long long)calldata_int(cd, "code"));
	os_event_signal(stopped_event);
}

static bool parse_args(int argc, char **argv, struct spike_args *a)
{
#if defined(_WIN32)
	a->graphics = "libobs-d3d11";
#else
	a->graphics = "libobs-opengl";
#endif
	a->video_encoder = "obs_x264";
	a->audio_encoder = "ffmpeg_opus";
	a->seconds = 3;
	a->modules = "image-source,obs-x264,obs-ffmpeg,obs-outputs,mac-videotoolbox";

	for (int i = 1; i < argc; i++) {
		const char *v = i + 1 < argc ? argv[i + 1] : NULL;
		if (!v) {
			fprintf(stderr, "missing value for %s\n", argv[i]);
			return false;
		}
		if (strcmp(argv[i], "--graphics") == 0)
			a->graphics = v;
		else if (strcmp(argv[i], "--video-encoder") == 0)
			a->video_encoder = v;
		else if (strcmp(argv[i], "--audio-encoder") == 0)
			a->audio_encoder = v;
		else if (strcmp(argv[i], "--plugin-bin") == 0)
			a->plugin_bin = v;
		else if (strcmp(argv[i], "--plugin-data") == 0)
			a->plugin_data = v;
		else if (strcmp(argv[i], "--modules") == 0)
			a->modules = v;
		else if (strcmp(argv[i], "--seconds") == 0)
			a->seconds = atoi(v);
		else {
			fprintf(stderr, "unknown argument %s\n", argv[i]);
			return false;
		}
		i++;
	}
	return true;
}

#define FAIL(...)                                    \
	do {                                         \
		fprintf(stderr, "SPIKE FAIL: " __VA_ARGS__); \
		fprintf(stderr, "\n");               \
		rc = 1;                              \
		goto cleanup;                        \
	} while (0)

int main(int argc, char **argv)
{
	struct spike_args args = {0};
	int rc = 0;
	obs_source_t *color = NULL;
	obs_scene_t *scene = NULL;
	obs_encoder_t *venc = NULL;
	obs_encoder_t *aenc = NULL;
	obs_output_t *output = NULL;
	bool started = false;
	bool initialized = false;

	setvbuf(stdout, NULL, _IONBF, 0);
	if (!parse_args(argc, argv, &args))
		return 2;

	base_set_log_handler(log_handler, NULL);
	if (os_event_init(&stopped_event, OS_EVENT_TYPE_MANUAL) != 0)
		return 1;

	if (!obs_startup("en-US", NULL, NULL))
		FAIL("obs_startup");
	initialized = true;
	printf("SPIKE libobs version %s\n", obs_get_version_string());

	struct obs_video_info ovi = {
		.graphics_module = args.graphics,
		.fps_num = 30,
		.fps_den = 1,
		.base_width = 1280,
		.base_height = 720,
		.output_width = 1280,
		.output_height = 720,
		.output_format = VIDEO_FORMAT_NV12,
		.adapter = 0,
		.gpu_conversion = true,
		.colorspace = VIDEO_CS_709,
		.range = VIDEO_RANGE_PARTIAL,
		.scale_type = OBS_SCALE_BICUBIC,
	};
	int vres = obs_reset_video(&ovi);
	printf("SPIKE obs_reset_video graphics=%s result=%d\n", args.graphics, vres);
	if (vres != OBS_VIDEO_SUCCESS)
		FAIL("obs_reset_video returned %d", vres);

	struct obs_audio_info oai = {.samples_per_sec = 48000, .speakers = SPEAKERS_STEREO};
	if (!obs_reset_audio(&oai))
		FAIL("obs_reset_audio");

	if (args.plugin_bin && args.plugin_data)
		obs_add_module_path(args.plugin_bin, args.plugin_data);

	// Frontend plugins such as obs-websocket and frontend-tools abort without a QApplication.
	char *mods = bstrdup(args.modules);
	for (char *m = strtok(mods, ","); m; m = strtok(NULL, ","))
		obs_add_safe_module(m);
	bfree(mods);

	struct obs_module_failure_info mfi;
	obs_load_all_modules2(&mfi);
	for (size_t i = 0; i < mfi.count; i++)
		printf("SPIKE module failed to load: %s\n", mfi.failed_modules[i]);
	obs_module_failure_info_free(&mfi);
	obs_post_load_modules();
	obs_log_loaded_modules();

	const char *id;
	for (size_t i = 0; obs_enum_encoder_types(i, &id); i++)
		printf("SPIKE encoder-type: %s codec=%s caps=0x%x\n", id, obs_get_encoder_codec(id),
		       obs_get_encoder_caps(id));

	obs_register_output(&spike_output_info);

	obs_data_t *cs = obs_data_create();
	obs_data_set_int(cs, "color", 0xFFC04020);
	obs_data_set_int(cs, "width", 1280);
	obs_data_set_int(cs, "height", 720);
	color = obs_source_create("color_source", "spike color", cs, NULL);
	obs_data_release(cs);
	if (!color)
		FAIL("color_source create");
	printf("SPIKE color source id=%s\n", obs_source_get_id(color));

	scene = obs_scene_create("spike scene");
	if (!scene)
		FAIL("scene create");
	obs_scene_add(scene, color);
	obs_set_output_source(0, obs_scene_get_source(scene));

	obs_data_t *vs = obs_data_create();
	obs_data_set_int(vs, "bitrate", 2500);
	obs_data_set_string(vs, "rate_control", "CBR");
	obs_data_set_int(vs, "keyint_sec", 1);
	obs_data_set_int(vs, "bf", 0);
	obs_data_set_string(vs, "profile", "main");
	venc = obs_video_encoder_create(args.video_encoder, "spike video", vs, NULL);
	obs_data_release(vs);
	if (!venc)
		FAIL("video encoder %s create", args.video_encoder);
	obs_encoder_set_video(venc, obs_get_video());

	obs_data_t *as = obs_data_create();
	obs_data_set_int(as, "bitrate", 128);
	aenc = obs_audio_encoder_create(args.audio_encoder, "spike audio", as, 0, NULL);
	obs_data_release(as);
	if (!aenc)
		FAIL("audio encoder %s create", args.audio_encoder);
	obs_encoder_set_audio(aenc, obs_get_audio());

	output = obs_output_create(spike_output_info.id, "spike output", NULL, NULL);
	if (!output)
		FAIL("output create");
	obs_output_set_video_encoder(output, venc);
	obs_output_set_audio_encoder(output, aenc, 0);
	signal_handler_connect(obs_output_get_signal_handler(output), "stop", on_output_stop, NULL);

	if (!obs_output_start(output)) {
		const char *err = obs_output_get_last_error(output);
		FAIL("obs_output_start: %s", err ? err : "(no error)");
	}
	started = true;

	os_sleep_ms((uint32_t)args.seconds * 1000);

	printf("SPIKE audio encoder frame_size=%zu sample_rate=%u\n", obs_encoder_get_frame_size(aenc),
	       obs_encoder_get_sample_rate(aenc));
	uint8_t *extra = NULL;
	size_t extra_size = 0;
	if (obs_encoder_get_extra_data(venc, &extra, &extra_size) && extra_size) {
		hex("video extra_data", extra, extra_size, 96);
		describe_h264("video extra_data", extra, extra_size);
	} else {
		printf("SPIKE video extra_data: none\n");
	}
	if (obs_encoder_get_extra_data(aenc, &extra, &extra_size) && extra_size)
		hex("audio extra_data", extra, extra_size, 32);
	else
		printf("SPIKE audio extra_data: none\n");

	obs_output_stop(output);
	if (os_event_timedwait(stopped_event, 10000) != 0)
		FAIL("output did not stop within 10 s");
	started = false;

	printf("SPIKE totals video=%ld keyframes=%ld pts_ne_dts=%ld audio=%ld\n", video_packets, video_keyframes,
	       video_pts_ne_dts, audio_packets);
	if (video_packets == 0 || audio_packets == 0)
		FAIL("no packets (video=%ld audio=%ld)", video_packets, audio_packets);

cleanup:
	if (output && started)
		obs_output_force_stop(output);
	obs_output_release(output);
	obs_encoder_release(venc);
	obs_encoder_release(aenc);
	if (initialized) {
		obs_set_output_source(0, NULL);
		obs_scene_release(scene);
		obs_source_release(color);
		printf("SPIKE calling obs_shutdown\n");
		obs_shutdown();
	}
	printf("SPIKE shutdown complete, leaked allocations=%ld\n", bnum_allocs());
	os_event_destroy(stopped_event);
	printf("SPIKE result=%s\n", rc == 0 ? "PASS" : "FAIL");
	return rc;
}
