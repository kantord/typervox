import os
import sys
import json
import time
import signal
import queue
from pathlib import Path
from typing import Optional


# --- Simple state management (pid only) ---

APP_NAME = "typervox"


def _cache_dir() -> Path:
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg) if xdg else Path.home() / ".cache"
    d = base / APP_NAME
    d.mkdir(parents=True, exist_ok=True)
    return d


def _state_path() -> Path:
    return _cache_dir() / "state.json"


def _read_state():
    p = _state_path()
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except Exception:
        return None


def _write_state(data: dict):
    _state_path().write_text(json.dumps(data))


def _clear_state():
    try:
        _state_path().unlink()
    except FileNotFoundError:
        pass


def _process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except Exception:
        return False


def is_running() -> bool:
    state = _read_state()
    if not state:
        return False
    pid = int(state.get("pid", 0))
    if pid <= 0 or not _process_alive(pid):
        _clear_state()
        return False
    return True


# --- Transcription and typing ---

_shutdown_at: float | None = None


def _signal_finish(signum, frame):  # noqa: ARG001
    global _shutdown_at
    _shutdown_at = time.time() + 2.0


def _install_signal_handlers():
    if hasattr(signal, "SIGUSR1"):
        signal.signal(signal.SIGUSR1, _signal_finish)
    signal.signal(signal.SIGTERM, _signal_finish)
    signal.signal(signal.SIGINT, _signal_finish)


def _transcription_loop(model_name: Optional[str] = None):
    _install_signal_handlers()
    try:
        try:
            import numpy as np
            import sounddevice as sd
            from faster_whisper import WhisperModel
        except Exception as e:
            print(f"Missing or failed audio/ASR deps: {e}", file=sys.stderr)
            return

        selected_model = model_name or os.environ.get("TYPERVOX_MODEL", "small")
        try:
            model = WhisperModel(selected_model, device="cpu", compute_type="int8")
        except Exception as e:
            print(f"Failed to load model '{selected_model}': {e}", file=sys.stderr)
            return

        sr = 16000
        q: queue.Queue = queue.Queue(maxsize=32)

        def on_audio(indata, frames, time_info, status):  # noqa: ARG001
            try:
                q.put_nowait(indata.copy())
            except queue.Full:
                pass

        try:
            stream = sd.InputStream(samplerate=sr, channels=1, dtype="float32", callback=on_audio)
        except Exception as e:
            print(f"Audio input error: {e}", file=sys.stderr)
            return

        chunk_sec = 3.0
        chunk_samples = int(sr * chunk_sec)
        buf = []
        have = 0

        # Try to set up X11 typing; if not available, fall back to printing
        typer = None
        try:
            from Xlib import X, XK, display
            from Xlib.ext import xtest

            class _X11Typer:
                def __init__(self):
                    self.d = display.Display()
                    self.shift_kc = self._keysym_to_kc(XK.string_to_keysym("Shift_L"))
                    self.altgr_kc = self._keysym_to_kc(XK.string_to_keysym("ISO_Level3_Shift"))
                    if self.altgr_kc == 0:
                        self.altgr_kc = self._keysym_to_kc(XK.string_to_keysym("Mode_switch"))
                    self.map = self._build_map()

                def _keysym_to_kc(self, ks):
                    try:
                        return self.d.keysym_to_keycode(ks)
                    except Exception:
                        return 0

                def _build_map(self):
                    m = {}
                    try:
                        min_kc = self.d.display.min_keycode
                        max_kc = self.d.display.max_keycode
                    except Exception:
                        min_kc, max_kc = 8, 255
                    for kc in range(min_kc, max_kc + 1):
                        for level in range(4):
                            try:
                                ks = self.d.keycode_to_keysym(kc, level)
                            except Exception:
                                ks = 0
                            if not ks:
                                continue
                            if level == 0:
                                mods = []
                            elif level == 1 and self.shift_kc:
                                mods = [self.shift_kc]
                            elif level == 2 and self.altgr_kc:
                                mods = [self.altgr_kc]
                            elif level == 3 and self.shift_kc and self.altgr_kc:
                                mods = [self.altgr_kc, self.shift_kc]
                            else:
                                continue
                            m.setdefault(ks, (kc, mods))
                    return m

                def _press_release(self, kc, mods):
                    for m_kc in mods:
                        xtest.fake_input(self.d, X.KeyPress, m_kc)
                    xtest.fake_input(self.d, X.KeyPress, kc)
                    xtest.fake_input(self.d, X.KeyRelease, kc)
                    for m_kc in reversed(mods):
                        xtest.fake_input(self.d, X.KeyRelease, m_kc)
                    self.d.sync()

                def type_text(self, text: str):
                    for ch in text:
                        if ch == " ":
                            ks = XK.string_to_keysym("space")
                        elif ch in ("\n", "\r"):
                            ks = XK.string_to_keysym("Return")
                        elif ch == "\t":
                            ks = XK.string_to_keysym("Tab")
                        else:
                            ks = XK.string_to_keysym(ch)
                            if not ks:
                                ks = 0x01000000 | ord(ch)
                        entry = self.map.get(ks)
                        if entry is None:
                            kc = self._keysym_to_kc(ks)
                            if kc:
                                self._press_release(kc, [])
                            continue
                        kc, mods = entry
                        if kc:
                            self._press_release(kc, mods)

                def close(self):
                    try:
                        self.d.close()
                    except Exception:
                        pass

            typer = _X11Typer()
        except Exception:
            typer = None

        with stream:
            while True:
                if _shutdown_at is not None and time.time() >= _shutdown_at:
                    break
                try:
                    block = q.get(timeout=0.1)
                except queue.Empty:
                    continue
                if block.ndim > 1:
                    block = block[:, 0]
                buf.append(block)
                have += len(block)
                if have >= chunk_samples:
                    audio = None
                    try:
                        import numpy as np
                        audio = np.concatenate(buf, axis=0).astype("float32", copy=False)
                    except Exception:
                        pass
                    buf.clear()
                    have = 0
                    if audio is None:
                        continue
                    try:
                        segments, info = model.transcribe(
                            audio,
                            language="en",
                            beam_size=1,
                            vad_filter=False,
                            without_timestamps=True,
                        )
                        text = "".join(seg.text for seg in segments).strip()
                        if text:
                            if typer is not None:
                                typer.type_text(text)
                            else:
                                print(text, flush=True)
                    except Exception as e:
                        print(f"Transcription error: {e}", file=sys.stderr)
                        time.sleep(0.2)
                        continue
    finally:
        try:
            if 'typer' in locals() and typer is not None:
                typer.close()
        finally:
            _clear_state()


def start(model: Optional[str] = None) -> bool:
    if is_running():
        return False
    _write_state({"pid": os.getpid()})
    _transcription_loop(model_name=model)
    return True


def request_stop():
    state = _read_state()
    if not state:
        return
    pid = int(state.get("pid", 0))
    if pid <= 0 or not _process_alive(pid):
        _clear_state()
        return
    try:
        if hasattr(signal, "SIGUSR1"):
            os.kill(pid, signal.SIGUSR1)
        else:
            os.kill(pid, signal.SIGTERM)
    except Exception:
        if not _process_alive(pid):
            _clear_state()


def status_message() -> str | None:
    return "Voice typing" if is_running() else None

