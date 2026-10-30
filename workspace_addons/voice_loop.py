import sys
import wave
from pathlib import Path
from pywhispercpp.model import Model
from piper import PiperVoice

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
VOICE_PATH = WORKSPACE / "voices" / "en_US-lessac-medium.onnx"

def transcribe_audio(audio_path):
    print(f"Transcribing {audio_path} using Whisper.cpp...")
    model = Model('tiny', print_realtime=False, print_progress=False)
    segments = model.transcribe(str(audio_path))
    full_text = " ".join([seg.text for seg in segments])
    print(f"Transcription: '{full_text}'")
    return full_text

def speak_response(text):
    print(f"Synthesizing speech: '{text}'")
    voice = PiperVoice.load(str(VOICE_PATH))
    output_wav = WORKSPACE / "voice_reply.wav"
    with open(output_wav, "wb") as f:
        voice.synthesize(text, f)
    print(f"Audio reply saved to {output_wav}")
    return output_wav

if __name__ == "__main__":
    print("Orion Voice Loop Initialized (STT + TTS ready).")
