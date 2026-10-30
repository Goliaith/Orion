import http.server
import socketserver
import json
import subprocess
from pathlib import Path

PORT = 7314
WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
TTS_SCRIPT = WORKSPACE / "orion_tts.py"
AUDIO_FILE = WORKSPACE / "output.wav"
AUTO_APPROVE_STATE_FILE = Path("/home/mrsilhouette/.local/state/orion/auto_approve_mode")

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
        elif self.path == "/toggle_auto_approve":
            content_length = int(self.headers.get('Content-Length', 0))
            post_data = self.rfile.read(content_length)
            try:
                data = json.loads(post_data.decode('utf-8'))
                enabled = bool(data.get("enabled", False))
                AUTO_APPROVE_STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
                AUTO_APPROVE_STATE_FILE.write_text("1" if enabled else "0")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(json.dumps({"status": "success", "auto_approve": enabled}).encode())
            except Exception as e:
                self.send_response(500)
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(str(e).encode())
        elif self.path == "/restart_servers":
            try:
                subprocess.run(["pkill", "-f", "orion_tts_server.py"], capture_output=True)
                subprocess.Popen(["python3", str(WORKSPACE / "orion_tts_server.py")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                subprocess.run(["orion", "shell", "--restart"], capture_output=True)
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(json.dumps({"status": "success"}).encode())
            except Exception as e:
                self.send_response(500)
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(str(e).encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_GET(self):
        if self.path == "/status":
            auto_approve = False
            if AUTO_APPROVE_STATE_FILE.exists():
                auto_approve = AUTO_APPROVE_STATE_FILE.read_text().strip() == "1"
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(json.dumps({"auto_approve": auto_approve}).encode())
        elif self.path == "/" or self.path == "/index.html":
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            html = """<!DOCTYPE html>
<html>
<head>
    <title>Orion Voice & Auto-Approve Companion</title>
    <style>
        body { font-family: sans-serif; background: #121212; color: #e0e0e0; padding: 40px; max-width: 800px; margin: auto; }
        textarea { width: 100%; height: 100px; background: #1e1e1e; color: #fff; border: 1px solid #333; padding: 12px; font-size: 16px; border-radius: 6px; }
        button { background: #3b82f6; color: white; border: none; padding: 12px 24px; font-size: 16px; border-radius: 6px; cursor: pointer; margin-top: 15px; }
        button:hover { background: #2563eb; }
        .danger-btn { background: #ef4444; }
        .danger-btn:hover { background: #dc2626; }
        .card { background: #1e1e1e; border: 1px solid #333; padding: 20px; border-radius: 8px; margin-bottom: 20px; }
        .switch { position: relative; display: inline-block; width: 60px; height: 34px; vertical-align: middle; margin-left: 15px; }
        .switch input { opacity: 0; width: 0; height: 0; }
        .slider { position: absolute; cursor: pointer; top: 0; left: 0; right: 0; bottom: 0; background-color: #ccc; transition: .4s; border-radius: 34px; }
        .slider:before { position: absolute; content: ""; height: 26px; width: 26px; left: 4px; bottom: 4px; background-color: white; transition: .4s; border-radius: 50%; }
        input:checked + .slider { background-color: #10b981; }
        input:checked + .slider:before { transform: translateX(26px); }
        #status, #approvalStatus, #serverStatus { margin-top: 15px; font-weight: bold; }
    </style>
</head>
<body>
    <h1>Orion Control Companion</h1>
    
    <div class="card">
        <h3>Auto-Approve Mode Toggle</h3>
        <p>When enabled, Orion sets system policy status to auto-approve routine sandbox operations.</p>
        <label>Auto-Approve Mode:</label>
        <label class="switch">
            <input type="checkbox" id="autoApproveToggle" onchange="toggleAutoApprove()">
            <span class="slider"></span>
        </label>
        <div id="approvalStatus">Loading status...</div>
        <br>
        <button class="danger-btn" onclick="restartServers()">Restart Servers</button>
        <div id="serverStatus"></div>
    </div>

    <div class="card">
        <h3>Orion Neural Voice Companion</h3>
        <textarea id="textInput">Hello MrSilhouette. Orion browser voice companion with auto-approve toggle is active and ready.</textarea><br>
        <button onclick="speakText()">Speak Aloud</button>
        <div id="status"></div>
    </div>

    <script>
        async function fetchStatus() {
            try {
                const res = await fetch('/status');
                const data = await res.json();
                document.getElementById('autoApproveToggle').checked = data.auto_approve;
                document.getElementById('approvalStatus').innerText = data.auto_approve ? 'Status: Auto-Approve Enabled (Active)' : 'Status: Auto-Approve Disabled (Standard)';
            } catch (err) {
                console.error(err);
            }
        }

        async function toggleAutoApprove() {
            const enabled = document.getElementById('autoApproveToggle').checked;
            try {
                const res = await fetch('/toggle_auto_approve', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ enabled: enabled })
                });
                const data = await res.json();
                document.getElementById('approvalStatus').innerText = data.auto_approve ? 'Status: Auto-Approve Enabled (Active)' : 'Status: Auto-Approve Disabled (Standard)';
            } catch (err) {
                alert('Failed to update toggle: ' + err);
            }
        }

        async function restartServers() {
            document.getElementById('serverStatus').innerText = 'Restarting servers...';
            try {
                const res = await fetch('/restart_servers', { method: 'POST' });
                if (res.ok) {
                    document.getElementById('serverStatus').innerText = 'Servers restarted successfully!';
                } else {
                    document.getElementById('serverStatus').innerText = 'Failed to restart servers.';
                }
            } catch (err) {
                document.getElementById('serverStatus').innerText = 'Error: ' + err;
            }
        }

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

        fetchStatus();
    </script>
</body>
</html>"""
            self.wfile.write(html.encode('utf-8'))
        else:
            self.send_response(404)
            self.end_headers()

class ReusableTCPServer(socketserver.TCPServer):
    allow_reuse_address = True

if __name__ == "__main__":
    if not AUTO_APPROVE_STATE_FILE.exists():
        AUTO_APPROVE_STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
        AUTO_APPROVE_STATE_FILE.write_text("1")
    with ReusableTCPServer(("127.0.0.1", PORT), TTSServerHandler) as httpd:
        print(f"Orion TTS Web Companion running on http://127.0.0.1:{PORT}")
        httpd.serve_forever()
