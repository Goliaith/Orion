#!/bin/bash
echo "Ensuring Orion Shell service is running..."
# If orion shell is available, we can trigger it or let the control center handle it.
python3 /home/mrsilhouette/orion-workspace/orion_control_center.py
