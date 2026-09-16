"""Finite live-hoist exercise: motion, Preferences lifecycle, then a static tail.

Run via run-godot-hoist --app blender --blender-script PATH --seconds 75.
Only this factory-startup process is changed; nothing is saved to disk.
"""
import math
import time

import bpy
from mathutils import Quaternion

started = None
opened = False
closed = False
reported = -1


def exercise():
    global started, opened, closed, reported
    now = time.monotonic()
    if started is None:
        started = now
        print("WELD_BLENDER_EXERCISE start", flush=True)
    elapsed = now - started
    windows = list(bpy.context.window_manager.windows)
    if elapsed >= 20 and not opened:
        with bpy.context.temp_override(window=windows[0]):
            bpy.ops.screen.userpref_show("INVOKE_DEFAULT")
        opened = True
        print("WELD_BLENDER_EXERCISE preferences-open", flush=True)
    if elapsed >= 40 and not closed:
        for window in windows:
            if any(area.type == "PREFERENCES" for area in window.screen.areas):
                with bpy.context.temp_override(window=window):
                    bpy.ops.wm.window_close()
        closed = True
        print("WELD_BLENDER_EXERCISE preferences-close", flush=True)
        windows = list(bpy.context.window_manager.windows)
    if elapsed >= 55:
        print("WELD_BLENDER_EXERCISE stopped-motion", flush=True)
        return None
    for window in windows:
        for area in window.screen.areas:
            if area.type == "VIEW_3D":
                region = area.spaces.active.region_3d
                region.view_rotation = Quaternion((0, 0, 1), elapsed * 1.8) @ Quaternion((1, 0, 0), 1.1)
                region.view_distance = 7.0 + math.sin(elapsed * 2.0)
                area.tag_redraw()
    second = int(elapsed)
    if second // 5 != reported:
        reported = second // 5
        print(f"WELD_BLENDER_EXERCISE elapsed={elapsed:.1f} windows={len(windows)}", flush=True)
    return 1.0 / 90.0


bpy.app.timers.register(exercise, first_interval=5.0)
