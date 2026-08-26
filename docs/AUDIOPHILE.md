# Audiophile playback

Youta's audiophile goal is an observable, low-noise signal path with dependable
playback. It does not claim that Rust, lower RAM use, a fixed CPU frequency, or
a special theme changes audio quality by itself.

## What matters

For digital playback, the practical targets are:

- no buffer underruns or audible dropouts;
- no unintended sample-rate, sample-format, channel, or codec conversion;
- no unintended DSP, mixing, clipping, or software volume;
- correct device selection and stable clocking at the audio interface;
- enough buffering for the hardware and workload;
- measurements and logs instead of assumed improvements.

Once decoded samples reach the audio device correctly and on time, ordinary
scheduler timing variation is absorbed by buffers. “Minimal jitter” at a DAC is
primarily a hardware/driver/clocking concern, not the variation in when the TUI
draws a frame.

## Youta signal-path profiles

The planned profiles are transparent configuration presets, not hidden tuning:

### `balanced` (default)

- automatic output and device selection;
- `mpv` default audio buffer;
- software volume and speed controls available;
- subscription refresh and low-priority background work allowed;
- status reports the active output, device, input/output format, and filters.

### `direct`

- explicit output and device required, for example an ALSA `hw:` device;
- software volume defaults to 100%;
- equalizer, loudness normalization, crossfade, speed change, and pitch
  correction are disabled;
- source sample rate/format is requested from the device;
- background thumbnail, waveform, subscription, and download work pauses while
  playing;
- any conversion changes the status from `direct` to `converted` and explains
  why.

### `low-latency`

This is for monitoring and interactive use, not automatically better listening.
It requests a smaller buffer and may increase wakeups, CPU use, dropouts, heat,
and fan activity. The selected buffer and underrun count must be visible.

## ALSA

`mpv` can select ALSA explicitly:

```toml
[playback]
output = 'alsa'
device = 'alsa/hw:0,0'
```

List actual devices with:

```sh
mpv --audio-device=help
aplay -L
```

An ALSA `hw:` device avoids the `dmix` plugin and generally requires exclusive
access. It also refuses formats the hardware does not accept. `plughw:` is more
convenient because ALSA may convert format, rate, or channels; that conversion
means the path is not bit-perfect. Device names differ, so Youta must never
guess that `hw:0,0` is the desired DAC.

Do not force a small ALSA buffer by folklore. The `mpv` manual notes that lower
buffers can increase CPU use and dropouts, while larger buffers increase
control latency. Start with defaults, inspect underruns, and adjust one setting
at a time.

`mpv --audio-exclusive=yes` does not add ALSA exclusivity: according to the mpv
manual, ALSA does not implement that option. Selecting a direct `hw:` PCM is the
relevant ALSA mechanism.

Reference:

- [mpv audio options and output drivers](https://mpv.io/manual/master/#audio)
- [ALSA configuration](https://www.alsa-project.org/wiki/Asoundrc)
- [Linux ALSA PCM timestamping](https://www.kernel.org/doc/html/latest/sound/designs/timestamping.html)

## Sample rates, codecs, and large formats

Youta should preserve the source stream to the decoder and avoid lossy
transcoding for local playback. Opus, AAC/M4A, MP3, Ogg Vorbis, and many video
audio tracks are lossy; FLAC is compressed lossless; WAV is commonly
uncompressed PCM. A larger file or higher numeric sample rate is not evidence
of a better master.

The output device ultimately receives decoded PCM. If it cannot accept the
source rate or format, conversion is necessary somewhere. Youta's diagnostics
should show:

```text
source: FLAC, 24-bit, 96 kHz, stereo
decoder output: float 32-bit, 96 kHz, stereo
device: ALSA hw:2,0
device output: signed 32-bit, 96 kHz, stereo
filters: none
```

Playback speed other than 1.0× necessarily changes timing and normally applies
a pitch-correction filter. Equalization and non-100% software volume also
modify samples. These features are useful, but direct-mode status must turn off
when they are active.

Downloaded YouTube audio is already encoded by YouTube. Converting it to FLAC
or WAV increases size without restoring information. Keep the original Opus
stream when possible. For Commons transfer, remux an eligible existing Opus
stream rather than decode/re-encode it when the container requirements permit.

## Estimating source quality

A file exposes its current encoding, not a trustworthy history. A FLAC stream
can contain samples decoded from MP3, and a nominal 320 kb/s MP3 can be made
from a 128 kb/s MP3. Neither wrapper reveals that earlier encoder setting, so
Youta's explicit `[V] Analyze quality` action reports evidence rather than a
“real bitrate.” It accepts one selected audio file, a selected folder, or the
currently marked files and folders. Folder batches are traversed
deterministically and analyzed one file at a time; their bounded progress popup
keeps completed rows available to copy.

The analyzer examines active FFT windows from up to the leading 30 seconds,
calculates Hann-windowed spectra, and looks for a stable sharp high-frequency
cutoff with a quiet band above it. It reports that measured evidence without
converting it into a codec-neutral bitrate class: Opus, AAC, MP3, Vorbis, and
other encoders apply different low-pass behaviour. A stable cutoff can still
support a cautious `band-limited` assessment, but it is a heuristic:

- old recordings, speech, dark masters, and deliberate low-pass filtering can
  produce the same cutoff without any lossy ancestor;
- some high-quality and modern lossy encoders retain broad bandwidth, so an
  earlier encode can evade this test;
- sample rate, bit depth, file size, and a FLAC integrity checksum do not prove
  that the source master was lossless;
- when the current sample rate or channel count is unavailable, or a stream
  with more than two channels must be normalized to stereo, Youta retains
  cutoff evidence but suppresses source-history inference;
- the output labels its assessment as heuristic, gives evidence strength and
  window agreement, and never calls an exact earlier bitrate recovered, an
  up-encode proven, or a file genuine.

More codec-specific research can strengthen later detectors—for example the
[AES Lossless Audio Checker paper](https://aes.org/publications/elibrary-page/?id=17972)—but
it does not make provenance cryptographically recoverable. Youta performs the
current FFT analysis locally with [RustFFT](https://docs.rs/rustfft/) over PCM
decoded by the configured FFmpeg helper.

## CPU frequency and “monotonic CPU usage”

Youta will not set a CPU governor or pin a frequency.

Linux CPUFreq behavior depends on the scaling driver, hardware coordination,
thermal limits, and power policy. Even the userspace governor cannot guarantee
an exact physical frequency. Locking a processor at a high frequency can raise
power use, temperature, and fan noise. Locking it too low can cause decode
deadlines to be missed. Modern `schedutil` is designed to respond in scheduler
context with low overhead.

There is no portable evidence that frequency transitions improve or degrade
decoded samples when playback is not underrunning. If a user wants to
experiment, measure first:

1. record current governor and driver;
2. play a representative high-rate file while other expected tasks run;
3. count player/ALSA underruns and observe scheduling latency and temperature;
4. change one external system setting;
5. repeat and retain the change only if the measurement improves.

Youta's diagnostics may report the current governor read-only. It does not
write sysfs, request root access, disable C-states, isolate CPUs, or raise
real-time priority.

Reference: [Linux CPU performance
scaling](https://www.kernel.org/doc/html/latest/admin-guide/pm/cpufreq.html).

## Real-time scheduling and kernel configuration

A general media player should not request `SCHED_FIFO` by default. A runaway
real-time thread can starve desktop, storage, network, and even recovery work.
The mature playback backend and ALSA already buffer ordinary scheduling
variation.

For a measured studio/monitoring workload, users may separately evaluate:

- a current kernel and audio driver;
- `PREEMPT_DYNAMIC`/low-latency distribution kernels;
- IRQ placement and an appropriate PipeWire/JACK quantum;
- real-time limits managed by the distribution's audio group policy.

These can reduce worst-case latency, but they are system administration, not
sound-quality presets. Youta will document observed backend latency and
underruns before suggesting any change. No custom kernel is needed for normal
music listening.

## Heat, battery, and silent operation

Low wakeup frequency often helps battery life and fan noise more than a small
binary alone. During playback Youta should:

- consume `mpv` property-change events instead of polling rapidly;
- draw position at a modest configurable interval;
- coalesce the 30-second persistence update;
- pause thumbnail, waveform, index, download, and subscription workers in
  direct mode;
- avoid animations in the default theme;
- retain compressed media rather than transcode during playback;
- use bounded caches and release decoded artwork.

Compile-time feature selection reduces dependency count, binary size, attack
surface, and sometimes startup work. It does not make the active decoder's
audio inherently cleaner.

Example minimal direct-playback build:

```sh
cargo build --release --no-default-features \
	--features tui,local,backend-mpv,alsa
```

## Jitter, latency, and quality terminology

- **Latency** is time from a control/input to audible output. Larger buffers
  increase it.
- **Scheduling jitter** is variation in when software runs. A sufficient audio
  buffer prevents it from becoming an underrun.
- **Clock jitter** is timing variation at digital conversion/transport and is
  chiefly controlled by audio hardware and its clock recovery.
- **Bit-perfect** means the intended digital samples reach the device without
  DSP or conversion. It does not guarantee that two devices or analog paths
  perform identically.
- **Gapless** means adjacent tracks play without an unintended inserted gap;
  it requires correct codec delay/padding and queue behavior.

Youta should use these terms precisely in the UI and documentation.

## Verification checklist

Before calling a path direct:

- [ ] explicit output and device selected;
- [ ] source and device sample rates match;
- [ ] source and device channel layouts match;
- [ ] no resampler or channel-conversion filter;
- [ ] speed is 1.0×;
- [ ] equalizer, normalization, crossfade, and pitch filters are off;
- [ ] software volume is 100% or hardware volume is intentionally selected;
- [ ] no underruns reported during the listening session;
- [ ] the device is not being mixed by an unexpected layer.

Youta can automate inspection of the player properties, but hardware and
system-mixer behavior may still require an external loopback or device-specific
test.
