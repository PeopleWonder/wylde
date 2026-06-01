@echo off
REM Phase 8.5 — delete the now-empty Core\harness\tooling\tools\n8n\ folder.
REM
REM Run this by double-clicking it from File Explorer (the agent's bash
REM sandbox can't delete files under the workspace mount).
REM
REM Why this exists: the seven n8n tools (n8n_list_workflows, n8n_get_workflow,
REM n8n_get_execution, n8n_execute_workflow, n8n_create_workflow,
REM n8n_edit_workflow, n8n_delete_workflow) have been hoisted from
REM   Core\harness\tooling\tools\n8n\<tool>\
REM to their owning service at
REM   N8N\tools\<tool>\
REM under the new principle: services that provide LLM-callable tools host
REM them inside the service's folder, same convention as Extensions. The
REM tool_registry walker now scans Wylde\<Service>\tools\**\manifest.json
REM in addition to the harness tools\ tree, so the catalog still includes
REM the seven tools — they just live in their natural home.
REM
REM This .bat removes the leftover empty group folder
REM (only __init__.py + __pycache__\ remain after the move).
REM
REM Safe to run twice; the rd command no-ops if the folder is gone.

setlocal
set "TARGET=%~dp0Core\harness\tooling\tools\n8n"

if not exist "%TARGET%" (
    echo Already gone: %TARGET%
    goto :done
)

echo Deleting: %TARGET%
rd /s /q "%TARGET%"

if exist "%TARGET%" (
    echo FAILED — folder still present. May be locked by an open editor or shell.
    pause
    exit /b 1
)

echo Done.

:done
echo.
echo This .bat is itself part of Phase 8.5 cleanup. You can delete it now.
pause
