"""Settings read from the environment."""

import os

BACKUP_DIR = os.environ.get("NOTES_BACKUP_DIR", "/var/lib/notes/backup")
