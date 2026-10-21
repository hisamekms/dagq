"""The service's requests: each one checks its user's token first."""

from notes.store import NoteStore

store = NoteStore()


def authorized(request, tokens):
    token = request.headers.get("Authorization", "")
    return token != "" and tokens.get(token) == request.user


def add_note(request, tokens):
    if not authorized(request, tokens):
        return 403, "forbidden"
    count = store.add(request.user, request.body)
    return 201, str(count)


def list_notes(request, tokens):
    if not authorized(request, tokens):
        return 403, "forbidden"
    return 200, "\n".join(store.list(request.user))
