import pytest

from notes.store import NoteStore


def test_a_note_is_listed_after_it_is_added():
    store = NoteStore()
    assert store.add("ann", "buy milk") == 1
    assert store.list("ann") == ["buy milk"]


def test_an_empty_note_is_refused():
    store = NoteStore()
    with pytest.raises(ValueError):
        store.add("ann", "  ")


def test_users_do_not_see_each_others_notes():
    store = NoteStore()
    store.add("ann", "buy milk")
    assert store.list("bob") == []
