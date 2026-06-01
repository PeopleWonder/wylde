"""tools/fs/ — filesystem tools.

Read, write, edit, and list operations on the local filesystem. No sandbox;
the caller is responsible for path safety. Reads are size-capped (100 KiB)
to keep large files from drowning an LLM context.
"""
