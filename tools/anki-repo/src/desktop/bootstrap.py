
# BEGIN ANKI DECK TOOLKIT BOOTSTRAP 1
try:
    from pathlib import Path as _toolkit_Path
    _toolkit_file = _toolkit_Path(__file__).parent / 'user_files' / 'toolkit' / 'payload.py'
    exec(compile(_toolkit_file.read_bytes(), str(_toolkit_file), 'exec'), {'upstream': globals(), '__file__': str(_toolkit_file)})
except Exception as _toolkit_error:
    print('Anki Deck Toolkit: ошибка bootstrap:', _toolkit_error)
# END ANKI DECK TOOLKIT BOOTSTRAP 1
