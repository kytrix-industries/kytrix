#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) in vec2 position;
layout(location = 0) out vec2 v_world_px;
layout(location = 1) out vec2 v_uv;

void main() {
    v_world_px = geometry_world(position);
    v_uv = vec2(0.0);
    gl_Position = vec4(to_ndc(v_world_px), 0.0, 1.0);
}
