import math
from build123d import *

diameter = 1.0 * 25.4            # 25.4 mm (Radius = 12.7 mm)
length = 4.0 * 25.4              # 101.6 mm
pitch = 3.0                      # Thread pitch
head_dia = diameter * 1.8        # ~45.7 mm
head_height = diameter * 0.7     # ~17.8 mm
socket_width = diameter * 0.45   # ~11.4 mm
socket_depth = head_height * 0.6 # Socket depth

print("Generating bolt with fixed-height constant-depth threads...")

with BuildPart() as bolt:
    # 1. Socket Head at top (UNTOUCHED)
    with Locations((0, 0, head_height / 2.0)):
        Cylinder(radius=head_dia / 2.0, height=head_height)

    # 2. Hex Socket Recess (UNTOUCHED)
    top_face = bolt.faces().sort_by(Axis.Z)[-1]
    with BuildSketch(top_face) as socket_sk:
        RegularPolygon(radius=socket_width / 2.0, side_count=6)
    extrude(socket_sk.sketch, amount=-socket_depth, mode=Mode.SUBTRACT)

    # 3. Main Shaft / Shank (UNTOUCHED)
    with Locations((0, 0, -length / 2.0)):
        Cylinder(radius=diameter / 2.0, height=length)

    # 4. Helix path locked precisely to shaft outer radius to prevent height/radial variation
    with BuildLine() as thread_path:
        Helix(pitch=pitch, height=-length, radius=diameter/2.0, lefthand=True)
    
    # Profile centered exactly at the shaft radius with fixed local coordinates to ensure constant projection height
    with BuildSketch(Plane(origin=(diameter/2.0, 0, 0), z_dir=(1,0,0))) as thread_profile:
        Polygon([(0, 0), (pitch*0.45, -pitch/2), (0, -pitch)])
    
    sweep(sections=thread_profile.sketch, path=thread_path.line, mode=Mode.ADD)

# Export models
export_stl_path = "/home/mrsilhouette/orion-workspace/bolt_standard_size.stl"
export_step_path = "/home/mrsilhouette/orion-workspace/bolt_standard_size.step"

export_stl(bolt.part, export_stl_path)
export_step(bolt.part, export_step_path)

print(f"Constant-height threaded standard bolt successfully generated and exported to:\n- {export_stl_path}\n- {export_step_path}")
