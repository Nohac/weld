"""Blender-side counters for the isolated orbit fixture; never saves settings."""

import time
import bpy

counts = {"delivered_motion": 0, "draw": 0}
started = time.monotonic()
bpy.context.preferences.view.show_splash = False


class WeldProbe(bpy.types.Operator):
    bl_idname = "wm.weld_probe"
    bl_label = "Weld profiling event counter"

    def invoke(self, context, event):
        context.window_manager.modal_handler_add(self)
        return {'RUNNING_MODAL'}

    def modal(self, context, event):
        if event.type in ('MOUSEMOVE', 'INBETWEEN_MOUSEMOVE'):
            counts['delivered_motion'] += 1
        return {'PASS_THROUGH'}


def draw():
    counts['draw'] += 1


def report():
    sizes = [(area.width, area.height) for window in bpy.context.window_manager.windows
             for area in window.screen.areas if area.type == 'VIEW_3D']
    print('WELD_PROBE', round(time.monotonic() - started, 3), dict(counts), sizes, flush=True)
    return 1.0


def start():
    bpy.utils.register_class(WeldProbe)
    bpy.ops.wm.weld_probe('INVOKE_DEFAULT')
    bpy.types.SpaceView3D.draw_handler_add(draw, (), 'WINDOW', 'POST_PIXEL')
    bpy.app.timers.register(report, first_interval=1.0)
    return None


bpy.app.timers.register(start, first_interval=1.0)
