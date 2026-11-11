"""``tools/component-shell``: the build tool is a script package, not an installed distribution.

This file exists so ``python3 -m unittest discover -s tools/component-shell`` (the task's acceptance
command) can start there: unittest refuses a start directory that is not importable, and a directory
holding ``__init__.py`` is the way it says so. The tool itself is used as ``build-shell.py`` with its
own directory on ``sys.path``; nothing imports this package.
"""
