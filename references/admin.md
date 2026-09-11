# Administration reference

Load this file for installation, MCP host setup, or browser review-console
work. Use `Installation.md` for the exact installation commands and host
authorization rules.

Use the installed package/runtime selected by the host. Do not silently switch
to a source checkout or edit a user's MCP configuration without authorization.

The browser console is for human review and handoff. It can display the
projection, collect accept/reject/defer decisions, and queue source-anchored
patches; it is not a DOCX writer. The agent applies the queue transactionally
and owns build/verify.

For release delivery, Microsoft Word/official DOCX-OOXML is the target:
require a Word open without repair prompts and Word rendering when a PDF is
needed. LibreOffice is optional and non-gating.
