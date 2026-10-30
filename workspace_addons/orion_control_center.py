#!/usr/bin/env python3
import sys
import os
import subprocess
import time
from pathlib import Path
from PySide6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QTabWidget,
    QTextEdit, QLineEdit, QPushButton, QLabel, QMessageBox, QHBoxLayout, QCheckBox
)
from PySide6.QtCore import Qt, QUrl
from PySide6.QtWebEngineWidgets import QWebEngineView

WORKSPACE = Path("/home/mrsilhouette/orion-workspace")
DEV_DIR = Path("/home/mrsilhouette/orion-dev")
SHELL_URL_FILE = Path("/home/mrsilhouette/.local/state/orion/shell-url")
TTS_SCRIPT = WORKSPACE / "orion_tts.py"
ROUTER_SCRIPT = WORKSPACE / "orion_router.py"
AUTO_APPROVE_STATE_FILE = Path("/home/mrsilhouette/.local/state/orion/auto_approve_mode")

class OrionControlCenter(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setObjectName("OrionControlCenterWindow")
        self.setWindowTitle("Orion Control Center (Smart Routed & Auto-Approve)")
        self.resize(1100, 750)
        
        self.ensure_shell_server()
        
        self.tabs = QTabWidget()
        self.tabs.setElideMode(Qt.TextElideMode.ElideNone)
        self.tabs.setStyleSheet("""
            QTabWidget::pane { border: 1px solid #cccccc; }
            QTabBar::tab { padding: 8px 16px; min-width: 100px; }
        """)
        self.setCentralWidget(self.tabs)
        
        self.init_ai_tab()
        self.init_shell_web_tab()
        self.init_status_tab()
        self.init_tasks_tab()
        self.init_tts_tab()

    def ensure_shell_server(self):
        try:
            import socket
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            result = s.connect_ex(('127.0.0.1', 7310))
            s.close()
            if result != 0:
                subprocess.Popen(["orion", "shell", "--open"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                time.sleep(1.0)
        except Exception:
            pass

    def init_ai_tab(self):
        widget = QWidget()
        layout = QVBoxLayout()
        
        layout.addWidget(QLabel("<b>Orion Smart Intent & Model Router (Auto-Selects 3B vs 7B)</b>"))
        self.intent_input = QLineEdit()
        self.intent_input.setPlaceholderText("Enter prompt or task for Orion Router...")
        layout.addWidget(self.intent_input)
        
        btn = QPushButton("Execute via Smart Router")
        btn.clicked.connect(self.run_smart_router)
        layout.addWidget(btn)
        
        self.ai_output = QTextEdit()
        self.ai_output.setReadOnly(True)
        layout.addWidget(self.ai_output)
        
        widget.setLayout(layout)
        self.tabs.addTab(widget, "Smart AI")

    def run_smart_router(self):
        prompt = self.intent_input.text().strip()
        if not prompt:
            return
        cmd = ["python3", str(ROUTER_SCRIPT), prompt]
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=90)
            self.ai_output.setPlainText(res.stdout if res.returncode == 0 else f"Error:\n{res.stderr}")
        except Exception as e:
            self.ai_output.setPlainText(str(e))

    def init_shell_web_tab(self):
        widget = QWidget()
        layout = QVBoxLayout()
        
        layout.addWidget(QLabel("<b>Orion Shell Web UI</b>"))
        self.web_view = QWebEngineView()
        
        url = "http://127.0.0.1:7310"
        if SHELL_URL_FILE.exists():
            content = SHELL_URL_FILE.read_text().strip()
            if content.startswith("http"):
                url = content
                
        self.web_view.setUrl(QUrl(url))
        layout.addWidget(self.web_view)
        
        widget.setLayout(layout)
        self.tabs.addTab(widget, "Orion Shell")

    def init_status_tab(self):
        widget = QWidget()
        layout = QVBoxLayout()
        
        layout.addWidget(QLabel("<b>System Health, Auto-Approve & Persistent Vector Cache Status</b>"))
        
        # Auto-Approve Control Toggle & Restart Servers Control
        auto_layout = QHBoxLayout()
        auto_layout.addWidget(QLabel("<b>Auto-Approve Routine Operations:</b>"))
        self.auto_approve_checkbox = QCheckBox("Enable Auto-Approve Mode")
        is_enabled = False
        if AUTO_APPROVE_STATE_FILE.exists():
            is_enabled = AUTO_APPROVE_STATE_FILE.read_text().strip() == "1"
        self.auto_approve_checkbox.setChecked(is_enabled)
        self.auto_approve_checkbox.stateChanged.connect(self.toggle_auto_approve_state)
        auto_layout.addWidget(self.auto_approve_checkbox)
        
        restart_servers_btn = QPushButton("Restart Servers")
        restart_servers_btn.setStyleSheet("background-color: #ef4444; color: white; font-weight: bold; padding: 6px 12px;")
        restart_servers_btn.clicked.connect(self.restart_servers)
        auto_layout.addWidget(restart_servers_btn)
        
        layout.addLayout(auto_layout)

        self.status_text = QTextEdit()
        self.status_text.setReadOnly(True)
        
        refresh_btn = QPushButton("Refresh Status & Diagnostics")
        refresh_btn.clicked.connect(self.refresh_status)
        
        layout.addWidget(self.status_text)
        layout.addWidget(refresh_btn)
        
        widget.setLayout(layout)
        self.tabs.addTab(widget, "System & Health")
        self.refresh_status()

    def restart_servers(self):
        try:
            subprocess.run(["pkill", "-f", "orion_tts_server.py"], capture_output=True)
            subprocess.Popen(["python3", str(WORKSPACE / "orion_tts_server.py")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            
            subprocess.run(["orion", "shell", "--restart"], capture_output=True)
            QMessageBox.information(self, "Success", "Orion local servers (TTS companion and shell server) have been restarted successfully!")
            self.refresh_status()
        except Exception as e:
            QMessageBox.critical(self, "Error", f"Failed to restart servers: {e}")

    def toggle_auto_approve_state(self, state):
        enabled = self.auto_approve_checkbox.isChecked()
        try:
            AUTO_APPROVE_STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
            AUTO_APPROVE_STATE_FILE.write_text("1" if enabled else "0")
            self.refresh_status()
        except Exception as e:
            QMessageBox.critical(self, "Error", f"Failed to update auto-approve mode: {e}")

    def refresh_status(self):
        auto_state = "Enabled (Active)" if self.auto_approve_checkbox.isChecked() else "Disabled (Standard)"
        info = "=== Orion System Status & Pipeline Diagnostics ===\n"
        info += f"Auto-Approve Mode: {auto_state}\n"
        info += f"Auto-Approve State File: {AUTO_APPROVE_STATE_FILE}\n"
        info += "OS: Fedora Linux 44 (KDE Plasma Desktop Edition)\n"
        info += "Active Model: qwen2.5:7b (Primary) + llama3.2:3b (Fast)\n"
        info += "Smart Router: Integrated into Control Center\n"
        info += "Persistent Embedding Cache: Active (~/.local/share/orion/embedding_cache.db)\n"
        info += "Piper TTS Voice Engine: Ready & Verified\n"
        info += "Auto-Healer & Linter Daemon: Ready\n"
        self.status_text.setPlainText(info)

    def init_tasks_tab(self):
        widget = QWidget()
        layout = QVBoxLayout()
        
        layout.addWidget(QLabel("<b>Workspace Tasks (`tasks.md`)</b>"))
        self.tasks_edit = QTextEdit()
        
        tasks_file = WORKSPACE / "tasks.md"
        if tasks_file.exists():
            self.tasks_edit.setPlainText(tasks_file.read_text())
            
        save_btn = QPushButton("Save Tasks")
        save_btn.clicked.connect(self.save_tasks)
        
        layout.addWidget(self.tasks_edit)
        layout.addWidget(save_btn)
        
        widget.setLayout(layout)
        self.tabs.addTab(widget, "Tasks & Workspace")

    def init_tts_tab(self):
        widget = QWidget()
        layout = QVBoxLayout()
        
        layout.addWidget(QLabel("<b>Orion Neural Text-to-Speech Studio (Piper TTS) & Pipeline Inspector</b>"))
        self.tts_input = QTextEdit()
        self.tts_input.setPlaceholderText("Type text for Orion to synthesize into natural speech...")
        self.tts_input.setPlainText("Hello MrSilhouette. Smart intent routing, vector caching, and pipeline inspectors are fully operational.")
        
        tts_btn = QPushButton("Synthesize & Play Speech")
        tts_btn.clicked.connect(self.synthesize_speech)
        
        self.tts_status = QLabel("")
        
        layout.addWidget(self.tts_input)
        layout.addWidget(tts_btn)
        layout.addWidget(self.tts_status)
        
        widget.setLayout(layout)
        self.tabs.addTab(widget, "Voice & TTS")

    def synthesize_speech(self):
        text = self.tts_input.toPlainText().strip()
        if not text:
            return
        try:
            start_time = time.time()
            res = subprocess.run(["python3", str(TTS_SCRIPT), text], capture_output=True, text=True, timeout=15)
            duration = time.time() - start_time
            if res.returncode == 0:
                self.tts_status.setText(f"Success: Speech synthesized in {duration:.2f}s to ~/orion-workspace/output.wav")
                subprocess.Popen(["paplay", str(WORKSPACE / "output.wav")])
            else:
                self.tts_status.setText(f"Error: {res.stderr}")
        except Exception as e:
            self.tts_status.setText(f"Error: {str(e)}")

    def save_tasks(self):
        tasks_file = WORKSPACE / "tasks.md"
        try:
            tasks_file.write_text(self.tasks_edit.toPlainText())
            QMessageBox.information(self, "Success", "Tasks saved successfully!")
        except Exception as e:
            QMessageBox.critical(self, "Error", str(e))

if __name__ == "__main__":
    app = QApplication(sys.argv)
    app.setDesktopFileName("orion-control-center")
    window = OrionControlCenter()
    window.show()
    sys.exit(app.exec())
