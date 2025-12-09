# typervox

CLI that captures audio from the ALSA `default` device and streams Whisper transcription to stdout.
On first run it auto-downloads a faster-whisper model via ct2rs Hub (default `Systran/faster-whisper-base.en`, English-only, INT8-friendly for low latency).

- Set `TVX_MODEL_ID` to choose a model ID for ct2rs Hub.
- Set `TVX_LANG` to fix the language (default `en`).
- Run `typervox` and watch recognized speech print incrementally.
