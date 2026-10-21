"""Keeps each user's notes in memory."""


class NoteStore:
    def __init__(self):
        self._notes = {}

    def add(self, user, text):
        if not text.strip():
            raise ValueError("a note needs text")
        self._notes.setdefault(user, []).append(text)
        return len(self._notes[user])

    def list(self, user):
        return list(self._notes.get(user, []))
