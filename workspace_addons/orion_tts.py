import sys
import wave
from pathlib import Path
from piper import PiperVoice

VOICE_PATH = Path("/home/mrsilhouette/orion-workspace/voices/en_US-lessac-medium.onnx")

def speak_to_file(text, output_path):
    if not VOICE_PATH.exists():
        print(f"Voice model not found at {VOICE_PATH}")
        return False
    
    voice = PiperVoice.load(str(VOICE_PATH))
    with wave.open(output_path, "wb") as wav_file:
        voice.synthesize_wav(text, wav_file)
    print(f"Generated speech audio saved to {output_path}")
    return True

if __name__ == "__main__":
    if len(sys.argv) > 1:
        text = " ".join(sys.argv[1:])
    else:
        text = "Hello MrSilhouette. Orion neural text-to-speech is fully operational."
    speak_to_file(text, "/home/mrsilhouette/orion-workspace/output.wav")
