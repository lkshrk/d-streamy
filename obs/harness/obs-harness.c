#include <inttypes.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef _WIN32
#include <sys/utsname.h>
#endif

#include <obs.h>
#include <util/base.h>
#include <util/bmem.h>
#include <util/dstr.h>
#include <util/platform.h>
#include <util/threading.h>
#include <util/util_uint64.h>

#define TRACE_OUTPUT_ID "dstreamy_trace_recorder"
#define AUDIO_SOURCE_ID "dstreamy_harness_audio"
#define TRACE_MAGIC "DSTRACE1"
#define PLUGIN_PANIC_LOG "[obs-dstreamy] panic caught in "

#define SAMPLE_RATE 48000
#define AUDIO_CHUNK 480
#define FLASH_PERIOD_NS 1000000000ULL
#define BEEP_NS 20000000ULL
#define AUDIO_BUFFERING_MS 200
#define BEEP_HZ 1000.0
#define TONE_HZ 440.0
#define BEEP_AMPLITUDE 0.5f
#define TONE_AMPLITUDE 0.05f
#define DARK_COLOR 0xFF202020
#define WHITE_COLOR 0xFFFFFFFF

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

struct harness_args {
	const char *graphics;
	const char *video_encoder;
	const char *audio_encoder;
	const char *output_id;
	const char *output_settings;
	const char *trace_dir;
	const char *plugin_bin;
	const char *plugin_data;
	const char *modules;
	const char *profile;
	const char *preset;
	const char *note;
	const char *x264opts;
	int seconds;
	int fps;
	int width;
	int height;
	int bitrate;
	int audio_bitrate;
	int bf;
	int keyint_sec;
	int expect_panics;
	bool bf_set;
	bool keyint_set;
	int argc;
	char **argv;
};

static struct harness_args args;
static volatile long obs_errors;
static volatile long plugin_panics_logged;
static int log_max_level = LOG_INFO;

// Written once before the audio thread and tick callback exist, so they read it without a lock.
static uint64_t marker_base_ns;

static os_event_t *stopped_event;
static os_event_t *full_event;

static void log_handler(int lvl, const char *msg, va_list va, void *param)
{
	UNUSED_PARAMETER(param);
	char buf[4096];
	vsnprintf(buf, sizeof(buf), msg, va);
	if (lvl <= LOG_ERROR) {
		if (strstr(buf, PLUGIN_PANIC_LOG))
			os_atomic_inc_long(&plugin_panics_logged);
		else
			os_atomic_inc_long(&obs_errors);
	}
	if (lvl > log_max_level)
		return;
	FILE *f = lvl <= LOG_ERROR ? stderr : stdout;
	fprintf(f, "[obs %s] %s\n", lvl <= LOG_ERROR ? "error" : lvl == LOG_WARNING ? "warn" : "info", buf);
	fflush(f);
}

static void harness_error(const char *fmt, ...)
{
	va_list va;
	va_start(va, fmt);
	fprintf(stderr, "HARNESS ERROR: ");
	vfprintf(stderr, fmt, va);
	fprintf(stderr, "\n");
	va_end(va);
}

struct flash_state {
	obs_source_t *color;
	uint64_t next_marker_ns;
	bool white;
};

static void flash_tick(void *param, float seconds)
{
	UNUSED_PARAMETER(seconds);
	struct flash_state *fs = param;
	uint64_t frame_end = obs_get_video_frame_time() + obs_get_frame_interval_ns();
	bool white = fs->next_marker_ns < frame_end;
	if (white) {
		while (fs->next_marker_ns < frame_end)
			fs->next_marker_ns += FLASH_PERIOD_NS;
	}
	if (white == fs->white)
		return;
	fs->white = white;
	obs_data_t *s = obs_data_create();
	obs_data_set_int(s, "color", white ? WHITE_COLOR : DARK_COLOR);
	obs_source_update(fs->color, s);
	obs_data_release(s);
}

struct audio_ctx {
	obs_source_t *source;
	os_event_t *stop;
	pthread_t thread;
	bool thread_started;
};

static float marker_sample(uint64_t t_ns, uint64_t sample_index)
{
	double tone = sin(2.0 * M_PI * TONE_HZ * (double)sample_index / SAMPLE_RATE) * TONE_AMPLITUDE;
	if (t_ns < marker_base_ns)
		return (float)tone;
	uint64_t rel = (t_ns - marker_base_ns) % FLASH_PERIOD_NS;
	if (rel >= BEEP_NS)
		return (float)tone;
	return (float)(sin(2.0 * M_PI * BEEP_HZ * (double)rel / 1e9) * BEEP_AMPLITUDE);
}

static void *audio_thread(void *param)
{
	struct audio_ctx *ctx = param;
	float samples[AUDIO_CHUNK];
	uint64_t start_ns = os_gettime_ns();
	uint64_t index = 0;

	os_set_thread_name("harness: audio");
	while (os_event_try(ctx->stop) == EAGAIN) {
		uint64_t chunk_ns = start_ns + util_mul_div64(index, 1000000000ULL, SAMPLE_RATE);
		for (size_t i = 0; i < AUDIO_CHUNK; i++) {
			uint64_t s = index + i;
			samples[i] = marker_sample(start_ns + util_mul_div64(s, 1000000000ULL, SAMPLE_RATE), s);
		}

		struct obs_source_audio data = {
			.data = {(const uint8_t *)samples, (const uint8_t *)samples},
			.frames = AUDIO_CHUNK,
			.speakers = SPEAKERS_STEREO,
			.format = AUDIO_FORMAT_FLOAT_PLANAR,
			.samples_per_sec = SAMPLE_RATE,
			.timestamp = chunk_ns,
		};
		obs_source_output_audio(ctx->source, &data);

		index += AUDIO_CHUNK;
		os_sleepto_ns(start_ns + util_mul_div64(index, 1000000000ULL, SAMPLE_RATE));
	}
	return NULL;
}

static const char *audio_source_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return "Harness marker audio";
}

static void audio_source_destroy(void *data)
{
	struct audio_ctx *ctx = data;
	if (ctx->thread_started) {
		os_event_signal(ctx->stop);
		pthread_join(ctx->thread, NULL);
	}
	os_event_destroy(ctx->stop);
	bfree(ctx);
}

static void *audio_source_create(obs_data_t *settings, obs_source_t *source)
{
	UNUSED_PARAMETER(settings);
	struct audio_ctx *ctx = bzalloc(sizeof(*ctx));
	ctx->source = source;
	if (os_event_init(&ctx->stop, OS_EVENT_TYPE_MANUAL) != 0) {
		bfree(ctx);
		return NULL;
	}
	if (pthread_create(&ctx->thread, NULL, audio_thread, ctx) != 0) {
		audio_source_destroy(ctx);
		return NULL;
	}
	ctx->thread_started = true;
	return ctx;
}

static struct obs_source_info audio_source_info = {
	.id = AUDIO_SOURCE_ID,
	.type = OBS_SOURCE_TYPE_INPUT,
	.output_flags = OBS_SOURCE_AUDIO,
	.get_name = audio_source_name,
	.create = audio_source_create,
	.destroy = audio_source_destroy,
};

struct trace_recorder {
	obs_output_t *output;
	FILE *packets;
	char *dir;
	pthread_mutex_t stop_mutex;
	pthread_mutex_t write_mutex;
	pthread_t stop_thread;
	bool stop_thread_joinable;
	bool video_extra_written;
	bool audio_extra_written;
	long video_limit;
	volatile long video_packets;
	volatile long audio_packets;
	volatile bool full;
	volatile bool write_failed;
};

static char *trace_path(const struct trace_recorder *rec, const char *name)
{
	struct dstr path = {0};
	dstr_printf(&path, "%s/%s", rec->dir, name);
	return path.array;
}

static bool write_file(const struct trace_recorder *rec, const char *name, const uint8_t *data, size_t size)
{
	char *path = trace_path(rec, name);
	FILE *f = os_fopen(path, "wb");
	bool ok = f && fwrite(data, 1, size, f) == size;
	if (f)
		ok = fclose(f) == 0 && ok;
	if (!ok)
		harness_error("cannot write %s", path);
	bfree(path);
	return ok;
}

static void put_u32(uint8_t *p, uint32_t v)
{
	for (int i = 0; i < 4; i++)
		p[i] = (uint8_t)(v >> (8 * i));
}

static void put_u64(uint8_t *p, uint64_t v)
{
	for (int i = 0; i < 8; i++)
		p[i] = (uint8_t)(v >> (8 * i));
}

static const char *trace_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return "D-Streamy trace recorder";
}

static void *trace_create(obs_data_t *settings, obs_output_t *output)
{
	struct trace_recorder *rec = bzalloc(sizeof(*rec));
	rec->output = output;
	rec->dir = bstrdup(obs_data_get_string(settings, "path"));
	rec->video_limit = (long)obs_data_get_int(settings, "video_packets");
	pthread_mutex_init(&rec->stop_mutex, NULL);
	pthread_mutex_init(&rec->write_mutex, NULL);
	return rec;
}

static void join_stop_thread(struct trace_recorder *rec)
{
	pthread_mutex_lock(&rec->stop_mutex);
	if (rec->stop_thread_joinable) {
		pthread_join(rec->stop_thread, NULL);
		rec->stop_thread_joinable = false;
	}
	pthread_mutex_unlock(&rec->stop_mutex);
}

static bool close_packets(struct trace_recorder *rec)
{
	pthread_mutex_lock(&rec->write_mutex);
	rec->full = true;
	bool ok = true;
	if (rec->packets) {
		ok = fflush(rec->packets) == 0 && !ferror(rec->packets);
		ok = fclose(rec->packets) == 0 && ok;
		rec->packets = NULL;
	}
	if (!ok)
		rec->write_failed = true;
	pthread_mutex_unlock(&rec->write_mutex);
	return ok && !rec->write_failed;
}

static void trace_destroy(void *data)
{
	struct trace_recorder *rec = data;
	join_stop_thread(rec);
	close_packets(rec);
	pthread_mutex_destroy(&rec->stop_mutex);
	pthread_mutex_destroy(&rec->write_mutex);
	bfree(rec->dir);
	bfree(rec);
}

static bool trace_start(void *data)
{
	struct trace_recorder *rec = data;
	if (!rec->dir || !*rec->dir || os_mkdirs(rec->dir) == MKDIR_ERROR) {
		obs_output_set_last_error(rec->output, "trace directory missing or not creatable");
		return false;
	}
	char *path = trace_path(rec, "packets.bin");
	rec->packets = os_fopen(path, "wb");
	bfree(path);
	if (!rec->packets || fwrite(TRACE_MAGIC, 1, 8, rec->packets) != 8) {
		obs_output_set_last_error(rec->output, "cannot create packets.bin");
		return false;
	}
	if (!obs_output_can_begin_data_capture(rec->output, 0))
		return false;
	if (!obs_output_initialize_encoders(rec->output, 0))
		return false;
	join_stop_thread(rec);
	return obs_output_begin_data_capture(rec->output, 0);
}

static void *trace_stop_thread(void *data)
{
	struct trace_recorder *rec = data;
	if (!close_packets(rec))
		harness_error("flushing packets.bin failed");
	obs_output_end_data_capture(rec->output);
	return NULL;
}

static void trace_stop(void *data, uint64_t ts)
{
	UNUSED_PARAMETER(ts);
	struct trace_recorder *rec = data;
	rec->full = true;
	pthread_mutex_lock(&rec->stop_mutex);
	if (!rec->stop_thread_joinable)
		rec->stop_thread_joinable = pthread_create(&rec->stop_thread, NULL, trace_stop_thread, data) == 0;
	pthread_mutex_unlock(&rec->stop_mutex);
}

static void write_extra_data(struct trace_recorder *rec, obs_encoder_t *encoder, const char *name)
{
	uint8_t *extra = NULL;
	size_t size = 0;
	if (!obs_encoder_get_extra_data(encoder, &extra, &size))
		size = 0;
	if (!write_file(rec, name, extra, size))
		rec->write_failed = true;
}

static void trace_packet(void *data, struct encoder_packet *pkt)
{
	struct trace_recorder *rec = data;
	if (!pkt) {
		harness_error("encoder error (NULL packet)");
		rec->full = true;
		obs_output_signal_stop(rec->output, OBS_OUTPUT_ENCODE_ERROR);
		os_event_signal(full_event);
		return;
	}
	pthread_mutex_lock(&rec->write_mutex);
	if (rec->full || !rec->packets) {
		pthread_mutex_unlock(&rec->write_mutex);
		return;
	}

	bool video = pkt->type == OBS_ENCODER_VIDEO;
	if (video && !rec->video_extra_written) {
		write_extra_data(rec, pkt->encoder, "video-extra.bin");
		rec->video_extra_written = true;
	} else if (!video && !rec->audio_extra_written) {
		write_extra_data(rec, pkt->encoder, "audio-extra.bin");
		rec->audio_extra_written = true;
	}

	uint8_t hdr[40];
	hdr[0] = video ? 1 : 0;
	hdr[1] = pkt->keyframe ? 1 : 0;
	hdr[2] = (uint8_t)pkt->priority;
	hdr[3] = (uint8_t)pkt->drop_priority;
	put_u32(hdr + 4, (uint32_t)pkt->timebase_num);
	put_u32(hdr + 8, (uint32_t)pkt->timebase_den);
	put_u32(hdr + 12, (uint32_t)pkt->size);
	put_u64(hdr + 16, (uint64_t)pkt->pts);
	put_u64(hdr + 24, (uint64_t)pkt->dts);
	put_u64(hdr + 32, (uint64_t)pkt->dts_usec);
	if (fwrite(hdr, 1, sizeof(hdr), rec->packets) != sizeof(hdr) ||
	    fwrite(pkt->data, 1, pkt->size, rec->packets) != pkt->size)
		rec->write_failed = true;

	if (!video) {
		os_atomic_inc_long(&rec->audio_packets);
	} else if (os_atomic_inc_long(&rec->video_packets) >= rec->video_limit) {
		rec->full = true;
		os_event_signal(full_event);
	}
	pthread_mutex_unlock(&rec->write_mutex);
}

static struct obs_output_info trace_output_info = {
	.id = TRACE_OUTPUT_ID,
	.flags = OBS_OUTPUT_AV | OBS_OUTPUT_ENCODED,
	.get_name = trace_name,
	.create = trace_create,
	.destroy = trace_destroy,
	.start = trace_start,
	.stop = trace_stop,
	.encoded_packet = trace_packet,
	.encoded_video_codecs = "h264",
	.encoded_audio_codecs = "opus;aac",
};

static volatile long long stop_code;

static void on_output_stop(void *param, calldata_t *cd)
{
	UNUSED_PARAMETER(param);
	long long code = calldata_int(cd, "code");
	if (code != OBS_OUTPUT_SUCCESS && stop_code == OBS_OUTPUT_SUCCESS)
		stop_code = code;
	if (code != OBS_OUTPUT_SUCCESS) {
		const char *err = calldata_string(cd, "last_error");
		harness_error("output stopped with code %lld: %s", code, err ? err : "(none)");
	}
	os_event_signal(stopped_event);
	os_event_signal(full_event);
}

static void usage(void)
{
	fprintf(stderr,
		"usage: obs-harness [--trace-dir DIR] [--seconds N] [--fps N] [--width N] [--height N]\n"
		"  [--video-encoder ID] [--audio-encoder ID] [--output ID] [--output-settings JSON] [--expect-panics N]\n"
		"  [--bitrate KBPS] [--audio-bitrate KBPS]\n"
		"  [--bf N] [--keyint-sec N] [--profile NAME] [--preset NAME] [--graphics MODULE]\n"
		"  [--modules a,b,c] [--plugin-bin DIR --plugin-data DIR] [--note TEXT] [--x264opts OPTS] [--verbose]\n");
}

static bool parse_int(const char *flag, const char *v, int min, int *out)
{
	char *end = NULL;
	long n = strtol(v, &end, 10);
	if (!*v || *end || n < min || n > 100000) {
		fprintf(stderr, "invalid value for %s: %s\n", flag, v);
		return false;
	}
	*out = (int)n;
	return true;
}

static int parse_args(int argc, char **argv)
{
#if defined(_WIN32)
	args.graphics = "libobs-d3d11";
#else
	args.graphics = "libobs-opengl";
#endif
	args.video_encoder = "obs_x264";
	args.audio_encoder = "ffmpeg_opus";
	args.output_id = TRACE_OUTPUT_ID;
	args.trace_dir = "trace";
	args.modules = "image-source,obs-x264,obs-ffmpeg,obs-outputs,mac-videotoolbox";
	args.seconds = 5;
	args.fps = 30;
	args.width = 1280;
	args.height = 720;
	args.bitrate = 2500;
	args.argc = argc;
	args.argv = argv;

	for (int i = 1; i < argc; i++) {
		const char *a = argv[i];
		if (strcmp(a, "--help") == 0) {
			usage();
			return 0;
		}
		if (strcmp(a, "--verbose") == 0) {
			log_max_level = LOG_DEBUG;
			continue;
		}
		const char *v = i + 1 < argc ? argv[++i] : NULL;
		if (!v) {
			fprintf(stderr, "missing value for %s\n", a);
			return 2;
		}
		bool ok = true;
		if (strcmp(a, "--graphics") == 0)
			args.graphics = v;
		else if (strcmp(a, "--video-encoder") == 0)
			args.video_encoder = v;
		else if (strcmp(a, "--audio-encoder") == 0)
			args.audio_encoder = v;
		else if (strcmp(a, "--output") == 0)
			args.output_id = v;
		else if (strcmp(a, "--output-settings") == 0)
			args.output_settings = v;
		else if (strcmp(a, "--expect-panics") == 0)
			ok = parse_int(a, v, 0, &args.expect_panics);
		else if (strcmp(a, "--trace-dir") == 0)
			args.trace_dir = v;
		else if (strcmp(a, "--plugin-bin") == 0)
			args.plugin_bin = v;
		else if (strcmp(a, "--plugin-data") == 0)
			args.plugin_data = v;
		else if (strcmp(a, "--modules") == 0)
			args.modules = v;
		else if (strcmp(a, "--profile") == 0)
			args.profile = v;
		else if (strcmp(a, "--preset") == 0)
			args.preset = v;
		else if (strcmp(a, "--note") == 0)
			args.note = v;
		else if (strcmp(a, "--x264opts") == 0)
			args.x264opts = v;
		else if (strcmp(a, "--seconds") == 0)
			ok = parse_int(a, v, 1, &args.seconds);
		else if (strcmp(a, "--fps") == 0)
			ok = parse_int(a, v, 1, &args.fps);
		else if (strcmp(a, "--width") == 0)
			ok = parse_int(a, v, 16, &args.width);
		else if (strcmp(a, "--height") == 0)
			ok = parse_int(a, v, 16, &args.height);
		else if (strcmp(a, "--bitrate") == 0)
			ok = parse_int(a, v, 1, &args.bitrate);
		else if (strcmp(a, "--audio-bitrate") == 0)
			ok = parse_int(a, v, 1, &args.audio_bitrate);
		else if (strcmp(a, "--bf") == 0)
			ok = args.bf_set = parse_int(a, v, 0, &args.bf);
		else if (strcmp(a, "--keyint-sec") == 0)
			ok = args.keyint_set = parse_int(a, v, 0, &args.keyint_sec);
		else {
			fprintf(stderr, "unknown argument %s\n", a);
			usage();
			return 2;
		}
		if (!ok)
			return 2;
	}
	if ((args.plugin_bin == NULL) != (args.plugin_data == NULL)) {
		fprintf(stderr, "--plugin-bin and --plugin-data go together\n");
		return 2;
	}
	return -1;
}

struct frame_counts {
	uint32_t lagged;
	uint32_t skipped;
};

static struct frame_counts frame_counts(void)
{
	struct frame_counts c = {obs_get_lagged_frames(), video_output_get_skipped_frames(obs_get_video())};
	return c;
}

static struct frame_counts frame_delta;

static bool check_output_stats(obs_output_t *output)
{
	calldata_t cd = {0};
	bool ok = proc_handler_call(obs_output_get_proc_handler(output), "get_stats", &cd);
	long long video = calldata_int(&cd, "video_packets");
	long long audio = calldata_int(&cd, "audio_packets");
	long long panics = calldata_int(&cd, "panics");
	calldata_free(&cd);
	if (!ok) {
		harness_error("output %s has no get_stats proc", args.output_id);
		return false;
	}
	printf("HARNESS output=%s video=%lld audio=%lld panics=%lld\n", args.output_id, video, audio, panics);
	if (stop_code != OBS_OUTPUT_SUCCESS)
		harness_error("output stopped with code %lld", stop_code);
	if (panics != args.expect_panics)
		harness_error("output counted %lld panic(s), expected %d", panics, args.expect_panics);
	if (!video || !audio)
		harness_error("output counted no video or no audio packets");
	return stop_code == OBS_OUTPUT_SUCCESS && panics == args.expect_panics && video && audio;
}

static void describe_os(struct dstr *out)
{
#ifdef _WIN32
	dstr_copy(out, "Windows");
#else
	struct utsname u;
	if (uname(&u) == 0)
		dstr_printf(out, "%s %s %s", u.sysname, u.release, u.machine);
	else
		dstr_copy(out, "unknown");
#endif
}

static bool write_provenance(obs_encoder_t *venc, obs_encoder_t *aenc, const struct trace_recorder *rec)
{
	struct dstr path = {0};
	dstr_printf(&path, "%s/PROVENANCE.md", args.trace_dir);
	FILE *f = os_fopen(path.array, "wb");
	dstr_free(&path);
	if (!f)
		return false;

	char date[32];
	time_t now = time(NULL);
	strftime(date, sizeof(date), "%Y-%m-%dT%H:%M:%SZ", gmtime(&now));
	struct dstr os = {0};
	describe_os(&os);

	obs_data_t *vs = obs_encoder_get_settings(venc);
	obs_data_t *as = obs_encoder_get_settings(aenc);

	fprintf(f, "# OBS trace\n\n");
	fprintf(f, "Recorded by `obs/harness/obs-harness.c` with its trace recorder output.\n\n");
	fprintf(f, "| Field | Value |\n|---|---|\n");
	fprintf(f, "| libobs | %s |\n", obs_get_version_string());
	const char *commit = getenv("OBS_COMMIT");
	const char *run_id = getenv("GITHUB_RUN_ID");
	fprintf(f, "| OBS commit | %s |\n", commit && *commit ? commit : "unknown");
	fprintf(f, "| CI run | %s |\n", run_id && *run_id ? run_id : "local");
	fprintf(f, "| OS | %s |\n", os.array);
	fprintf(f, "| Date (UTC) | %s |\n", date);
	fprintf(f, "| Graphics module | `%s` |\n", args.graphics);
	fprintf(f, "| Video | %dx%d at %d fps, NV12, Rec. 709 partial range |\n", args.width, args.height, args.fps);
	fprintf(f, "| Audio | 48000 Hz stereo |\n");
	fprintf(f, "| Video encoder | `%s` (codec `%s`) |\n", obs_encoder_get_id(venc), obs_encoder_get_codec(venc));
	fprintf(f, "| Audio encoder | `%s` (codec `%s`), frame size %zu |\n", obs_encoder_get_id(aenc),
		obs_encoder_get_codec(aenc), obs_encoder_get_frame_size(aenc));
	fprintf(f, "| Packets | %ld video, %ld audio |\n", rec->video_packets, rec->audio_packets);
	fprintf(f, "| Frames lagged / skipped while recording | %u / %u |\n", frame_delta.lagged, frame_delta.skipped);
	if (args.note)
		fprintf(f, "| Note | %s |\n", args.note);
	fprintf(f, "\nCommand line:\n\n```\n");
	for (int i = 0; i < args.argc; i++)
		fprintf(f, "%s%s", i ? " " : "", i ? args.argv[i] : "obs-harness");
	fprintf(f, "\n```\n\nVideo encoder settings set by the harness:\n\n```json\n%s\n```\n\n",
		obs_data_get_json_pretty(vs));
	fprintf(f, "Video encoder settings including defaults:\n\n```json\n%s\n```\n\n",
		obs_data_get_json_pretty_with_defaults(vs));
	fprintf(f, "Audio encoder settings including defaults:\n\n```json\n%s\n```\n\n",
		obs_data_get_json_pretty_with_defaults(as));
	fprintf(f, "Scene: `color_source_v3` %dx%d, dark (0x%08X) except one white frame per second; "
		   "harness audio source: 1 kHz beep for 20 ms at each flash, 440 Hz tone at amplitude %.2f otherwise.\n\n",
		args.width, args.height, DARK_COLOR, TONE_AMPLITUDE);
	fprintf(f, "Files: `packets.bin` (every `encoded_packet` in arrival order until %ld video packets), "
		   "`video-extra.bin` and `audio-extra.bin` (`obs_encoder_get_extra_data` at each track's first packet), "
		   "`trace.json`.\n\n",
		rec->video_limit);
	fprintf(f, "`packets.bin`: the 8 bytes `%s`, then per packet a 40-byte little-endian header followed by the payload:\n"
		   "type u8 (0 audio, 1 video), keyframe u8, priority u8, drop_priority u8, timebase_num i32, "
		   "timebase_den i32, size u32, pts i64, dts i64, dts_usec i64.\n",
		TRACE_MAGIC);

	obs_data_release(vs);
	obs_data_release(as);
	dstr_free(&os);
	return fclose(f) == 0;
}

static bool write_trace_json(obs_encoder_t *venc, obs_encoder_t *aenc, const struct trace_recorder *rec)
{
	obs_data_t *d = obs_data_create();
	obs_data_set_string(d, "libobs", obs_get_version_string());
	obs_data_set_string(d, "video_encoder", obs_encoder_get_id(venc));
	obs_data_set_string(d, "video_codec", obs_encoder_get_codec(venc));
	obs_data_set_string(d, "audio_encoder", obs_encoder_get_id(aenc));
	obs_data_set_string(d, "audio_codec", obs_encoder_get_codec(aenc));
	obs_data_set_int(d, "fps", args.fps);
	obs_data_set_int(d, "width", args.width);
	obs_data_set_int(d, "height", args.height);
	obs_data_set_int(d, "sample_rate", SAMPLE_RATE);
	obs_data_set_int(d, "audio_frame_size", (long long)obs_encoder_get_frame_size(aenc));
	obs_data_set_int(d, "video_packets", rec->video_packets);
	obs_data_set_int(d, "audio_packets", rec->audio_packets);
	obs_data_set_bool(d, "bf_set", args.bf_set);
	obs_data_set_bool(d, "keyint_set", args.keyint_set);
	obs_data_set_int(d, "lagged_frames", frame_delta.lagged);
	obs_data_set_int(d, "skipped_frames", frame_delta.skipped);
	const char *commit = getenv("OBS_COMMIT");
	const char *run_id = getenv("GITHUB_RUN_ID");
	obs_data_set_string(d, "obs_commit", commit && *commit ? commit : "unknown");
	obs_data_set_string(d, "ci_run", run_id && *run_id ? run_id : "local");
	obs_data_t *vs = obs_encoder_get_settings(venc);
	obs_data_set_obj(d, "video_settings", vs);
	obs_data_release(vs);

	struct dstr path = {0};
	dstr_printf(&path, "%s/trace.json", args.trace_dir);
	bool ok = obs_data_save_json_pretty_safe(d, path.array, "tmp", NULL);
	dstr_free(&path);
	obs_data_release(d);
	return ok;
}

static obs_data_t *video_settings(void)
{
	obs_data_t *vs = obs_data_create();
	obs_data_set_int(vs, "bitrate", args.bitrate);
	obs_data_set_string(vs, "rate_control", "CBR");
	if (args.keyint_set)
		obs_data_set_int(vs, "keyint_sec", args.keyint_sec);
	if (args.bf_set)
		obs_data_set_int(vs, "bf", args.bf);
	if (args.profile)
		obs_data_set_string(vs, "profile", args.profile);
	if (args.preset)
		obs_data_set_string(vs, "preset", args.preset);
	if (args.x264opts)
		obs_data_set_string(vs, "x264opts", args.x264opts);
	return vs;
}

int main(int argc, char **argv)
{
	int rc = 0;
	bool initialized = false;
	bool started = false;
	obs_source_t *color = NULL;
	obs_source_t *audio = NULL;
	obs_scene_t *scene = NULL;
	obs_encoder_t *venc = NULL;
	obs_encoder_t *aenc = NULL;
	obs_output_t *output = NULL;
	struct flash_state flash = {0};
	bool tick_added = false;

	setvbuf(stdout, NULL, _IONBF, 0);
	int parsed = parse_args(argc, argv);
	if (parsed >= 0)
		return parsed;

	base_set_log_handler(log_handler, NULL);
	if (os_event_init(&stopped_event, OS_EVENT_TYPE_MANUAL) != 0 ||
	    os_event_init(&full_event, OS_EVENT_TYPE_MANUAL) != 0)
		return 1;

#define FAIL(...)                              \
	do {                                   \
		harness_error(__VA_ARGS__);    \
		rc = 1;                        \
		goto cleanup;                  \
	} while (0)

	if (!obs_startup("en-US", NULL, NULL))
		FAIL("obs_startup failed (Linux needs an X display, e.g. xvfb-run)");
	initialized = true;
	printf("HARNESS libobs %s\n", obs_get_version_string());

	struct obs_video_info ovi = {
		.graphics_module = args.graphics,
		.fps_num = (uint32_t)args.fps,
		.fps_den = 1,
		.base_width = (uint32_t)args.width,
		.base_height = (uint32_t)args.height,
		.output_width = (uint32_t)args.width,
		.output_height = (uint32_t)args.height,
		.output_format = VIDEO_FORMAT_NV12,
		.adapter = 0,
		.gpu_conversion = true,
		.colorspace = VIDEO_CS_709,
		.range = VIDEO_RANGE_PARTIAL,
		.scale_type = OBS_SCALE_BICUBIC,
	};
	int vres = obs_reset_video(&ovi);
	if (vres != OBS_VIDEO_SUCCESS)
		FAIL("obs_reset_video(%s) returned %d", args.graphics, vres);

	// Fixed headroom: with dynamic buffering a chunk delivered late on a loaded runner is mixed as silence.
	struct obs_audio_info2 oai = {
		.samples_per_sec = SAMPLE_RATE,
		.speakers = SPEAKERS_STEREO,
		.max_buffering_ms = AUDIO_BUFFERING_MS,
		.fixed_buffering = true,
	};
	if (!obs_reset_audio2(&oai))
		FAIL("obs_reset_audio2 failed");

	if (args.plugin_bin)
		obs_add_module_path(args.plugin_bin, args.plugin_data);

	char *mods = bstrdup(args.modules);
	for (char *m = strtok(mods, ","); m; m = strtok(NULL, ","))
		obs_add_safe_module(m);
	bfree(mods);

	struct obs_module_failure_info mfi;
	obs_load_all_modules2(&mfi);
	size_t failed = mfi.count;
	for (size_t i = 0; i < mfi.count; i++)
		harness_error("module failed to load: %s", mfi.failed_modules[i]);
	obs_module_failure_info_free(&mfi);
	if (failed)
		FAIL("%zu module(s) failed to load", failed);
	obs_post_load_modules();
	obs_log_loaded_modules();

	obs_register_source(&audio_source_info);
	obs_register_output(&trace_output_info);
	uint64_t interval_ns = obs_get_frame_interval_ns();
	// Mid-frame, so the beep - flash offset is about half a frame instead of anywhere in [0, 1) frames.
	marker_base_ns = obs_get_video_frame_time() + util_mul_div64(FLASH_PERIOD_NS, 1, interval_ns) * interval_ns +
			 interval_ns / 2;

	obs_data_t *cs = obs_data_create();
	obs_data_set_int(cs, "color", DARK_COLOR);
	obs_data_set_int(cs, "width", args.width);
	obs_data_set_int(cs, "height", args.height);
	color = obs_source_create("color_source_v3", "harness color", cs, NULL);
	obs_data_release(cs);
	if (!color)
		FAIL("color_source_v3 create failed");
	flash.color = color;
	flash.next_marker_ns = marker_base_ns;

	audio = obs_source_create(AUDIO_SOURCE_ID, "harness audio", NULL, NULL);
	if (!audio)
		FAIL("harness audio source create failed");

	scene = obs_scene_create("harness scene");
	if (!scene)
		FAIL("scene create failed");
	obs_scene_add(scene, color);
	obs_scene_add(scene, audio);
	obs_set_output_source(0, obs_scene_get_source(scene));

	obs_add_tick_callback(flash_tick, &flash);
	tick_added = true;

	obs_data_t *vs = video_settings();
	venc = obs_video_encoder_create(args.video_encoder, "harness video", vs, NULL);
	obs_data_release(vs);
	if (!venc)
		FAIL("video encoder %s create failed", args.video_encoder);
	obs_encoder_set_video(venc, obs_get_video());

	obs_data_t *as = obs_data_create();
	if (args.audio_bitrate)
		obs_data_set_int(as, "bitrate", args.audio_bitrate);
	aenc = obs_audio_encoder_create(args.audio_encoder, "harness audio", as, 0, NULL);
	obs_data_release(as);
	if (!aenc)
		FAIL("audio encoder %s create failed", args.audio_encoder);
	obs_encoder_set_audio(aenc, obs_get_audio());

	bool recording = strcmp(args.output_id, TRACE_OUTPUT_ID) == 0;
	obs_data_t *os = args.output_settings ? obs_data_create_from_json(args.output_settings) : obs_data_create();
	if (!os)
		FAIL("--output-settings is not a JSON object");
	obs_data_set_string(os, "path", args.trace_dir);
	obs_data_set_int(os, "video_packets", (long long)args.seconds * args.fps);
	output = obs_output_create(args.output_id, "harness output", os, NULL);
	obs_data_release(os);
	if (!output)
		FAIL("output %s create failed", args.output_id);
	obs_output_set_video_encoder(output, venc);
	obs_output_set_audio_encoder(output, aenc, 0);
	signal_handler_connect(obs_output_get_signal_handler(output), "stop", on_output_stop, NULL);

	if (!obs_output_start(output)) {
		const char *err = obs_output_get_last_error(output);
		const char *verr = obs_encoder_get_last_error(venc);
		const char *aerr = obs_encoder_get_last_error(aenc);
		FAIL("obs_output_start: %s (video encoder: %s, audio encoder: %s)", err ? err : "(no error)",
		     verr ? verr : "-", aerr ? aerr : "-");
	}
	started = true;
	printf("HARNESS obs_output_start=ok\n");
	struct frame_counts at_start = frame_counts();

	unsigned long budget_ms = (unsigned long)args.seconds * 1000 + 30000;
	if (!recording)
		os_event_timedwait(full_event, (unsigned long)args.seconds * 1000);
	else if (os_event_timedwait(full_event, budget_ms) != 0)
		FAIL("no %d s of video within %lu ms", args.seconds, budget_ms);

	struct frame_counts at_end = frame_counts();
	frame_delta.lagged = at_end.lagged - at_start.lagged;
	frame_delta.skipped = at_end.skipped - at_start.skipped;

	obs_output_stop(output);
	if (os_event_timedwait(stopped_event, 10000) != 0)
		FAIL("output did not stop within 10 s");
	for (int i = 0; obs_output_active(output); i++) {
		if (i == 1000)
			FAIL("output still active 10 s after its stop signal");
		os_sleep_ms(10);
	}
	started = false;

	if (!recording) {
		if (!check_output_stats(output))
			rc = 1;
		goto cleanup;
	}

	struct trace_recorder *rec = obs_obj_get_data(output);
	join_stop_thread(rec);
	printf("HARNESS packets video=%ld audio=%ld lagged=%u skipped=%u\n", rec->video_packets, rec->audio_packets,
	       frame_delta.lagged, frame_delta.skipped);
	if (stop_code != OBS_OUTPUT_SUCCESS)
		FAIL("output stopped with code %lld", stop_code);
	if (rec->write_failed || !close_packets(rec))
		FAIL("writing the trace failed");
	if (rec->video_packets < rec->video_limit)
		FAIL("only %ld of %ld video packets", rec->video_packets, rec->video_limit);
	if (!rec->audio_packets)
		FAIL("no audio packets");
	if (!write_provenance(venc, aenc, rec) || !write_trace_json(venc, aenc, rec))
		FAIL("writing PROVENANCE.md or trace.json failed");

cleanup:
	if (output && started)
		obs_output_force_stop(output);
	obs_output_release(output);
	obs_encoder_release(venc);
	obs_encoder_release(aenc);
	if (initialized) {
		if (tick_added)
			obs_remove_tick_callback(flash_tick, &flash);
		obs_set_output_source(0, NULL);
		// Sources on the main canvas outlive their last release until removed, racing obs_free_data.
		if (scene)
			obs_source_remove(obs_scene_get_source(scene));
		if (color)
			obs_source_remove(color);
		if (audio)
			obs_source_remove(audio);
		obs_scene_release(scene);
		obs_source_release(color);
		obs_source_release(audio);
		while (obs_wait_for_destroy_queue())
			;
		obs_shutdown();
	}
	os_event_destroy(stopped_event);
	os_event_destroy(full_event);

	long errors = os_atomic_load_long(&obs_errors);
	if (errors) {
		harness_error("libobs logged %ld error(s)", errors);
		rc = 1;
	}
	long panics = os_atomic_load_long(&plugin_panics_logged);
	if (panics != args.expect_panics) {
		harness_error("plugin logged %ld caught panic(s), expected %d", panics, args.expect_panics);
		rc = 1;
	}
	long libobs_leaks = bnum_allocs();
	printf("HARNESS shutdown libobs_leaks=%ld\n", libobs_leaks);
	if (initialized && libobs_leaks) {
		harness_error("%ld libobs allocation(s) leaked after obs_shutdown", libobs_leaks);
		rc = 1;
	}
	printf("HARNESS result=%s\n", rc == 0 ? "PASS" : "FAIL");
	return rc;
}
