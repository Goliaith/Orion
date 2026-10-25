import http.server
import socketserver
import json
import subprocess
from pathlib import Path

PORT = 7311
WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
TTS_SCRIPT = WORKSPACE / "orion_tts.py"
AUDIO_FILE = WORKSPACE / "output.wav"

class TTSServerHandler(http.server.SimpleHTTPRequestHandler):
    def do_POST(self):
        if self.path == "/speak":
            content_length = int(self.headers.get('Content-Length', 0))
            post_data = self.rfile.read(content_length)
            try:
                data = json.loads(post_data.decode('utf-8'))
                text = data.get("text", "").strip()
                if not text:
                    self.send_response(400)
                    self.send_header("Access-Control-Allow-Origin", "*")
                    self.end_headers()
                    self.wfile.write(b"No text provided")
                    return
                
                res = subprocess.run(["python3", str(TTS_SCRIPT), text], capture_output=True, text=True, timeout=15)
                if res.returncode == 0:
                    subprocess.Popen(["paplay", str(AUDIO_FILE)])
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Access-Control-Allow-Origin", "*")
                    self.end_headers()
                    self.wfile.write(json.dumps({"status": "success"}).encode())
                else:
                    self.send_response(500)
                    self.send_header("Access-Control-Allow-Origin", "*")
                    self.end_headers()
                    self.wfile.write(res.stderr.encode())
            except Exception as e:
                self.send_response(500)
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(str(e).encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_GET(self):
        if self.path == "/" or self.path == "/index.html":
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            html = """<!DOCTYPE html>
<html>
<head>
    <title>Orion Voice Companion</title>
    <style>
        body { font-family: sans-serif; background: #121212; color: #e0e0e0; padding: 40px; }
        textarea { width: 100%; height: 120px; background: #1e1e1e; color: #fff; border: 1px solid #333; padding: 12px; font-size: 16px; border-radius: 6px; }
        button { background: #3b82f6; color: white; border: none; padding: 12px 24px; font-size: 16px; border-radius: 6px; cursor: pointer; margin-top: 15px; }
        button:hover { background: #2563eb; }
        #status { margin-top: 15px; font-weight: bold; }
    </style>
</head>
<body>
    <h1>Orion Neural Voice Companion</h1>
    <p>Type text below to have Orion speak it aloud using local neural TTS:</p>
    <textarea id="textInput">Hello MrSilhouette. Orion browser voice companion is active and ready.</textarea><br>
    <button onclick="speakText()">Speak Aloud</button>
    <div id="status"></div>
    <script>
        async function speakText() {
            const text = document.getElementById('textInput').value;
            document.getElementById('status').innerText = 'Synthesizing voice...';
            try {
                const response = await fetch('/speak', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ text: text })
                });
                const result = await response.json();
                if (response.ok) {
                    document.getElementById('status').innerText = 'Success: Spoken aloud!';
                } else {
                    document.getElementById('status').innerText = 'Error: ' + JSON.stringify(result);
                }
            } catch (err) {
                document.getElementById('status').innerText = 'Error: ' + err;
            }
        }
    </script>
</body>
</html>"""
            self.wfile.write(html.encode('utf-8'))
        else:
            self.send_response(404)
            self.end_headers()

if __name__ == "__main__":
    with socketserver.TCPServer(("", PORT), TTSServerHandler) as httpd:
        print(f"Orion TTS Web Companion running on http://127.0.0.1:{PORT}")
        httpd.serve_forever()
