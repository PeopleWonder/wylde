"""LLM-callable memory tools.

Five tools the chat-turn driver exposes via the catalog:

* ``memory.long_term.save``  — write a global memory the model wants
  preserved across conversations.
* ``memory.workspace.save``  — write a workspace-scoped memory tied
  to the active workspace_id (caller passes it in).
* ``memory.update``          — revise an existing memory by id; old
  one is marked superseded, new one becomes active.
* ``memory.delete``          — remove a memory (and any predecessors
  in its supersession chain).
* ``memory.search``          — query memories by scope; lets the model
  read its own memory mid-turn.

These dispatch to the same backends as the harness pipe actions; the
duplicate is intentional — the pipe action surface is for GUI / mobile
/ external callers, the tool surface is for the LLM inside a turn.
"""
