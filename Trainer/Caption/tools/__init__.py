# Caption-owned LLM tools.
#
# Each subfolder is a tool: manifest.json + __init__.py + <tool_id>.py.
# The harness catalog walker discovers them by globbing
# Wylde/<Service>/.../tools/**/manifest.json — module dispatch happens via
# the manifest's `module` + `entrypoint` fields.
