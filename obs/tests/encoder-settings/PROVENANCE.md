# H.264 encoder settings at the OBS pin

One JSON file per H.264 encoder id at `obsproject/obs-studio@ba2f32b` (32.2.2): `defaults` as the encoder's `get_defaults` sets them, and `properties` with the names of the property sheet. `kind` says where a file comes from:

- `captured`: written by `obs-harness --dump-encoder-settings` on a CI runner where the encoder registers. `source` names the run.
- `source`: derived by hand from the cited lines, for encoders that can't register on the CI runners (NVENC is disabled in the CI build, VAAPI has no device, AMF and QSV have no hardware, VideoToolbox runs only on macOS).

Only the keys the D-Streamy rules read are guaranteed complete in `source` files. Defaults that depend on the device (NVENC `bf`, AMF `bf` above 1080p or 60 fps, the VAAPI device) are recorded as the common case, as the cited lines say.

obs-ci dumps every registered H.264 encoder and compares it with these files (`obs/tests/check-encoder-settings.py`). When it fails, copy the dumped file from the run's `obs-encoder-settings-<os>` artifact, set `kind` to `captured` and `source` to the run, check it against the encoder's source at the pin, and commit it.
