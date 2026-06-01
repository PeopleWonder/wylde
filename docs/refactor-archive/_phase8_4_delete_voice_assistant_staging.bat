@echo off
REM Phase 8.4 — delete VoiceAssistant\_wylde_voice_assistant staging folder.
REM
REM Run this by double-clicking it from File Explorer (direct shell access
REM from the agent is unreliable). It removes the entire staging folder
REM (~138 MB, mostly NLU model artifacts that are no longer needed now
REM that intent parsing has moved out of VoiceAssistant).
REM Safe to run twice; the rd command no-ops if the folder is gone.
REM
REM After this completes, the VoiceAssistant\ tree should be ~23 files:
REM   __init__.py, config.py, config.yaml, device_manager.py,
REM   download_models.py, manifest.json, pipeline.py, README.md,
REM   requirements.txt, run.py
REM   audio\{__init__,capture,vad,sfx,bargein}.py
REM   wake_word\{__init__,engine,record_samples,trainer}.py
REM   stt\{__init__,engine}.py
REM   tts\{__init__,engine}.py
REM
REM The original full source remains untouched at
REM   _legacy\core\wylde-voice-assistant\
REM per the Wylde user's untouched-archive rule.

setlocal
set "TARGET=%~dp0VoiceAssistant\_wylde_voice_assistant"

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
echo This .bat is itself part of Phase 8.4 cleanup. You can delete it now.
pause
