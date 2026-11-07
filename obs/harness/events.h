#pragma once

static pthread_mutex_t event_mutex = PTHREAD_MUTEX_INITIALIZER;

static void harness_report(const char *event, const char *fields, const char *format, ...)
{
	va_list values, legacy;
	va_start(values, format);
	va_copy(legacy, values);
	obs_data_t *data = obs_data_create();
	obs_data_set_string(data, "event", event);
	for (const char *text = format; (text = strchr(text, '=')); text++) {
		const char *start = text;
		while (start > format && (isalnum((unsigned char)start[-1]) || start[-1] == '_'))
			start--;
		const char *end = text + 1;
		if (*end == '%')
			continue;
		while (*end && !isspace((unsigned char)*end))
			end++;
		if (start == text || end == text + 1)
			continue;
		char key[64], value[128];
		size_t key_len = (size_t)(text - start), value_len = (size_t)(end - text - 1);
		if (key_len >= sizeof(key) || value_len >= sizeof(value))
			continue;
		memcpy(key, start, key_len);
		key[key_len] = 0;
		memcpy(value, text + 1, value_len);
		value[value_len] = 0;
		obs_data_set_string(data, key, value);
	}
	const char *field = fields;
	for (const char *spec = format; *spec; spec++) {
		if (*spec != '%' || spec[1] == '%')
			continue;
		char key[64];
		size_t length = strcspn(field, " ");
		if (length >= sizeof(key))
			break;
		memcpy(key, field, length);
		key[length] = 0;
		field += length + (field[length] == ' ');
		spec++;
		while (*spec && strchr("-+ #0.123456789", *spec))
			spec++;
		int longs = 0;
		while (*spec == 'l') {
			longs++;
			spec++;
		}
		bool size = *spec == 'z';
		if (size)
			spec++;
		if (*spec == 's') {
			const char *value = va_arg(values, const char *);
			obs_data_set_string(data, key, value ? value : "");
		} else if (*spec == 'p') {
			char value[32];
			snprintf(value, sizeof(value), "%p", va_arg(values, void *));
			obs_data_set_string(data, key, value);
		} else if (strchr("fge", *spec)) {
			obs_data_set_double(data, key, va_arg(values, double));
		} else if (strchr("uxX", *spec)) {
			unsigned long long value = size         ? va_arg(values, size_t)
						   : longs == 2 ? va_arg(values, unsigned long long)
						   : longs == 1 ? va_arg(values, unsigned long)
								: va_arg(values, unsigned int);
			obs_data_set_int(data, key, (long long)value);
		} else {
			long long value = longs == 2   ? va_arg(values, long long)
					  : longs == 1 ? va_arg(values, long)
						       : va_arg(values, int);
			obs_data_set_int(data, key, value);
		}
	}
	va_end(values);
	pthread_mutex_lock(&event_mutex);
	vprintf(format, legacy);
	printf("%s\n", obs_data_get_json(data));
	fflush(stdout);
	pthread_mutex_unlock(&event_mutex);
	va_end(legacy);
	obs_data_release(data);
}
