#!/usr/bin/env python3
"""
Helper script to generate the systemd user service file for the Orion Workspace Auto-Watcher.
Since systemd service files must be placed in ~/.config/systemd/user/, this script writes
the service file to ~/.config/systemd/user/orion-watcher.service (if writable) or outputs it.
"""
from pathlib import Path

SERVICE_CONTENT = """[Unit]
Description=Orion Workspace Auto-Watcher Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/python3 /home/mrsilhouette/orion-dev/workspace_watcher/watcher.py --interval 3.0
Restart=on-failure

[Install]
WantedBy=default.target
"""

def main():
    target_dir = Path.home() / ".config" / "systemd" / "user"
    target_dir.mkdir(parents=True, exist_ok=True)
    service_path = target_dir / "orion-watcher.service"
    
    try:
        service_path.write_text(SERVICE_CONTENT)
        print(f"Successfully wrote systemd user service to {service_path}")
        print("Run the following commands to enable and start it:")
        print("  systemctl --user daemon-reload")
        print("  systemctl --user enable --now orion-watcher.service")
    except Exception as e:
        print(f"Could not write directly: {e}")
        print("Here is the service content you can save to ~/.config/systemd/user/orion-watcher.service:")
        print(SERVICE_CONTENT)

if __name__ == "__main__":
    main()
