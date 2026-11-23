#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) in vec2 position;
layout(location = 1) in float coverage;
layout(location = 0) out vec2 v_world_px;
layout(location = 2) out float v_coverage;

void main() {
    v_world_px = geometry_world(position);
    v_coverage = coverage * root.frame.params.x;
    gl_Position = vec4(to_ndc(v_world_px), 0.0, 1.0);
}
