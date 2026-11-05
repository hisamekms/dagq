#!/usr/bin/env python3
"""Compares `dagq search` and the search clue of `dagq related` on SQLite FTS5
(trigram) with PostgreSQL pg_trgm (A1) and pg_bigm (A2), and the application
bm25 over A1's filter (A3, for reference). See ../team-version-search-spike.md.

    python3 -I spike.py --repo REPO --commit SHA --psql PATH --out DIR

The connection comes from the PG* environment variables. The script recreates
the table search_docs in that database, so point it at a throwaway one. It uses
only the standard library, reads no clock or random number for the CSVs, and
writes the time it took to DIR/timings.txt, which is not part of the comparison.
"""
import argparse
import csv
import decimal
import math
import os
import sqlite3
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
TRIGRAM = 3            # domain::search::TRIGRAM
MAX_TRIGRAMS = 200     # infrastructure::related::MAX_TRIGRAMS
MAX_QUERY_WORDS = 20   # domain::search::MAX_QUERY_WORDS
DUPLICATE_CANDIDATES = 5  # application::prompt::plan_review::DUPLICATE_CANDIDATES
SEARCH_WEIGHT = 4.0    # domain::related::SEARCH_WEIGHT
SEARCH_FLOOR = 0.1     # domain::related::SEARCH_FLOOR
TOP = 10
COLS = ["title", "description", "acceptance", "context", "text"]
DOC = "(title||E'\\n'||description||E'\\n'||acceptance||E'\\n'||context||E'\\n'||text)"
TD = "(title||E'\\n'||description)"
FTS_DDL = """CREATE VIRTUAL TABLE search_index USING fts5(
  kind UNINDEXED, ref UNINDEXED, task_id UNINDEXED, goal_id UNINDEXED,
  run_id UNINDEXED, status UNINDEXED, updated_at UNINDEXED,
  title, description, acceptance, context, text, tokenize = 'trigram')"""


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], check=True,
                          capture_output=True).stdout


# --- corpus ---------------------------------------------------------------

def corpus(repo, commit):
    """rows: dicts with rowid, kind, updated_at and the indexed columns."""
    raw = git(repo, "log", "--no-merges", "--format=%H%x01%cI%x01%ct%x01%B%x02", commit)
    commits = []
    for entry in raw.decode("utf-8").split("\x02"):
        entry = entry.strip("\n")
        if not entry:
            continue
        sha, iso, epoch, body = entry.split("\x01", 3)
        commits.append((int(epoch), sha, body))
    commits.sort()  # oldest first; the sha breaks a tie of the same second
    rows, by_sha = [], {}
    for n, (epoch, sha, body) in enumerate(commits, 1):
        subject, _, rest = body.partition("\n")
        row = {"rowid": 4 * n, "kind": "task", "updated_at": utc(epoch), "title": subject,
               "description": rest.strip("\n"), "acceptance": "", "context": "", "text": ""}
        rows.append(row)
        by_sha[sha] = row
    last = {}
    current = None
    log = git(repo, "-c", "core.quotepath=off", "log", "--no-merges", "--format=%x01%ct",
              "--name-only", commit, "--", "docs/adr")
    for line in log.decode("utf-8").splitlines():
        if line.startswith("\x01"):
            current = int(line[1:])
        elif line:
            last.setdefault(line, current)
    names = git(repo, "-c", "core.quotepath=off", "ls-tree", "--name-only", commit,
                "docs/adr/").decode("utf-8").splitlines()
    names = sorted(n for n in names if n.endswith(".md"))
    for n, name in enumerate(names, 1):
        body = git(repo, "show", f"{commit}:{name}").decode("utf-8")
        if body.startswith("---\n"):
            end = body.find("\n---\n", 4)
            if end >= 0:
                body = body[end + 5:]
        title = next((l[2:] for l in body.splitlines() if l.startswith("# ")), "")
        rows.append({"rowid": 4 * n + 2, "kind": "note", "updated_at": utc(last[name]),
                     "title": title, "description": "", "acceptance": "", "context": "",
                     "text": body})
    return rows, by_sha, len(commits), len(names)


def utc(epoch):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))


# --- query: domain::search::parse_terms and tokens ------------------------

class Rejected(Exception):
    pass


def tokens(query):
    out, i, n = [], 0, len(query)
    while i < n:
        c = query[i]
        if c.isspace():
            i += 1
        elif c in "()":
            out.append(("op", c))
            i += 1
        elif c == '"':
            j = query.find('"', i + 1)
            j = n if j < 0 else j
            phrase = query[i + 1:j]
            i = j + 1
            if phrase.strip():
                out.append(("term", phrase))
        else:
            j = i
            while j < n and not (query[j].isspace() or query[j] in '()"'):
                j += 1
            word = query[i:j]
            i = j
            out.append(("op", word) if word in ("AND", "OR", "NOT") else ("term", word))
    return out


def parse_terms(query):
    """(fts expression or None, long terms, short terms, tokens to rank-filter)."""
    toks = tokens(query)
    is_short = lambda t: len(t) < TRIGRAM
    terms = [t for k, t in toks if k == "term"]
    short = [t for t in terms if is_short(t)]
    long_ = [t for t in terms if not is_short(t)]
    if not terms:
        raise Rejected("the query has no term")
    if short and any(k == "op" and t != "AND" for k, t in toks):
        raise Rejected("short terms with OR, NOT or parentheses")
    if short:
        parts = [f'"{t}"' for k, t in toks if k == "term" and not is_short(t)]
    else:
        parts = [f'"{t}"' if k == "term" else t for k, t in toks]
    return (" ".join(parts) if parts else None), long_, short, toks


def boolean_tree(toks):
    """FTS5's grammar: juxtaposition binds tighter than NOT, then AND, then OR."""
    pos = [0]

    def peek():
        return toks[pos[0]] if pos[0] < len(toks) else (None, None)

    def prim():
        kind, value = peek()
        if kind == "term":
            pos[0] += 1
            return ("term", value)
        if (kind, value) == ("op", "("):
            pos[0] += 1
            inner = or_()
            if peek() != ("op", ")"):
                raise Rejected("unbalanced parentheses")
            pos[0] += 1
            return inner
        raise Rejected(f"syntax error near {value!r}")

    def seq():
        node = prim()
        while peek()[0] == "term" or peek() == ("op", "("):
            node = ("and", node, prim())
        return node

    def binary(next_rule, op, name):
        def rule():
            node = next_rule()
            while peek() == ("op", op):
                pos[0] += 1
                node = (name, node, next_rule())
            return node
        return rule

    not_ = binary(seq, "NOT", "not")
    and_ = binary(not_, "AND", "and")
    or_ = binary(and_, "OR", "or")
    node = or_()
    if pos[0] != len(toks):
        raise Rejected("syntax error at the end")
    return node


def lit(text):
    return "'" + text.replace("'", "''") + "'"


def like_escape(term):
    return term.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")


def pred_a1(term):
    return f"{DOC} ILIKE {lit('%' + like_escape(term) + '%')}"


def pred_a2(term):
    return f"lower({DOC}) LIKE likequery(lower({lit(term)}))"


def render(node, pred):
    if node[0] == "term":
        return pred(node[1])
    left, right = render(node[1], pred), render(node[2], pred)
    if node[0] == "and":
        return f"({left} AND {right})"
    if node[0] == "or":
        return f"({left} OR {right})"
    return f"({left} AND NOT {right})"


def where(toks, short, long_, pred):
    if short:
        return " AND ".join(pred(t) for t in short + long_)
    return render(boolean_tree(toks), pred)


# --- adapters --------------------------------------------------------------

class Psql:
    def __init__(self, path):
        self.path = path

    def run(self, sql):
        done = subprocess.run([self.path, "-X", "-q", "--csv", "-v", "ON_ERROR_STOP=1"],
                              input=sql.encode("utf-8"), capture_output=True)
        if done.returncode != 0:
            raise RuntimeError(done.stderr.decode("utf-8", "replace")[:2000])
        text = done.stdout.decode("utf-8")
        rows = list(csv.reader(text.splitlines(True)))
        return rows[1:] if rows else []


def load_pg(pg, rows):
    pg.run("DROP TABLE IF EXISTS search_docs;"
           "CREATE EXTENSION IF NOT EXISTS pg_trgm; CREATE EXTENSION IF NOT EXISTS pg_bigm;"
           "CREATE TABLE search_docs (rowid bigint PRIMARY KEY, kind text, updated_at text,"
           " title text, description text, acceptance text, context text, text text);")
    with tempfile.NamedTemporaryFile("w", suffix=".csv", delete=False, encoding="utf-8",
                                     newline="") as f:
        writer = csv.writer(f, quoting=csv.QUOTE_ALL)  # COPY reads an unquoted empty field as NULL
        for r in rows:
            writer.writerow([r["rowid"], r["kind"], r["updated_at"]] + [r[c] for c in COLS])
    try:
        pg.run(f"\\copy search_docs FROM {lit(f.name)} CSV\n")
    finally:
        os.unlink(f.name)
    pg.run(f"CREATE INDEX search_docs_doc_trgm ON search_docs USING gin ({DOC} gin_trgm_ops);"
           f"CREATE INDEX search_docs_td_trgm ON search_docs USING gin ({TD} gin_trgm_ops);"
           f"CREATE INDEX search_docs_doc_bigm ON search_docs USING gin (lower({DOC}) gin_bigm_ops);"
           f"CREATE INDEX search_docs_td_bigm ON search_docs USING gin (lower({TD}) gin_bigm_ops);"
           "ANALYZE search_docs;")


def load_sqlite(rows):
    db = sqlite3.connect(":memory:")
    db.execute(FTS_DDL)
    db.executemany(
        "INSERT INTO search_index(rowid, kind, ref, updated_at, title, description, acceptance,"
        " context, text) VALUES (?,?,?,?,?,?,?,?,?)",
        [(r["rowid"], r["kind"], r["rowid"], r["updated_at"], *[r[c] for c in COLS]) for r in rows])
    return db


def search_fts5(db, fts, short, tasks_only=False):
    """Every matching rowid in order (infrastructure::search::search)."""
    filters, values = (["kind = 'task'"] if tasks_only else []), []
    if fts:
        filters.append("search_index MATCH ?")
        values.append(fts)
    concat = " || char(10) || ".join(COLS)
    for s in short:
        filters.append(f"({concat}) LIKE ? ESCAPE '\\'")
        values.append("%" + like_escape(s) + "%")
    order = "bm25(search_index), rowid DESC" if fts else "updated_at DESC, rowid DESC"
    sql = f"SELECT rowid FROM search_index WHERE {' AND '.join(filters)} ORDER BY {order}"
    return [r[0] for r in db.execute(sql, values)]


def search_pg(pg, toks, short, long_, pred, similarity, tasks_only=False):
    filt = where(toks, short, long_, pred)
    if tasks_only:
        filt = f"kind = 'task' AND ({filt})"
    if long_:
        rank = " + ".join(f"{similarity}({lit(t)}, {DOC})::float8" for t in long_)
        order = f"{rank} DESC, rowid DESC"
    else:
        order = 'updated_at COLLATE "C" DESC, rowid DESC'
    # enable_seqscan off: the index path is what is measured, not the table scan.
    rows = pg.run(f"SET enable_seqscan = off; SELECT rowid FROM search_docs WHERE {filt} ORDER BY {order};")
    return [int(r[0]) for r in rows]


def bm25(rows_by_id, phrases, columns, candidates):
    """FTS5's bm25 (fts5_aux.c): weights 1, k1 1.2, b 0.75, D and avgdl in trigrams
    of all five columns, N and n over the whole index, hits over `columns`."""
    all_rows = list(rows_by_id.values())
    sizes = {r["rowid"]: sum(max(len(r[c]) - 2, 0) for c in COLS) for r in all_rows}
    avgdl = sum(sizes.values()) / len(all_rows)
    k1, b = 1.2, 0.75
    folded = fold_rows(rows_by_id, columns)
    scores = {rowid: 0.0 for rowid in candidates}
    for phrase in phrases:
        needle = phrase.lower()
        hits = {}
        for rowid, texts in folded.items():
            f = sum(count_overlapping(t, needle) for t in texts)
            if f:
                hits[rowid] = f
        n = len(hits)
        idf = math.log((len(all_rows) - n + 0.5) / (n + 0.5))
        if idf <= 0:
            idf = 1e-6
        for rowid in candidates:
            f = hits.get(rowid, 0)
            if f:
                scores[rowid] -= idf * (f * (k1 + 1)) / (f + k1 * (1 - b + b * sizes[rowid] / avgdl))
    return scores


_FOLD = {}


def fold_rows(rows_by_id, columns):
    key = tuple(columns)
    if key not in _FOLD:
        _FOLD[key] = {i: [r[c].lower() for c in columns if r[c]] for i, r in rows_by_id.items()}
    return _FOLD[key]


def count_overlapping(text, needle):
    count, at = 0, text.find(needle)
    while at >= 0:
        count += 1
        at = text.find(needle, at + 1)
    return count


# --- related ---------------------------------------------------------------

def trigrams(title):
    seen = []
    for i in range(len(title) - TRIGRAM + 1):
        window = title[i:i + TRIGRAM]
        if any(c.isspace() or c == '"' for c in window) or not any(c.isalnum() for c in window) \
                or window in seen:
            continue
        seen.append(window)
        if len(seen) == MAX_TRIGRAMS:
            break
    return seen


def round3(x):
    """(x*1000).round()/1000 with Rust's f64::round: 0.5 goes away from zero."""
    scaled = decimal.Decimal(x * 1000.0).quantize(decimal.Decimal(1), rounding=decimal.ROUND_HALF_UP)
    return float(scaled) / 1000.0


def clamp(x):
    return max(0.0, min(1.0, x))


def weight(strength):
    if strength < SEARCH_FLOOR:
        return 0.0
    return round3(SEARCH_WEIGHT * min(strength, 1.0))


def ranking(strengths, a_id):
    """domain::related::rank with the search clue only."""
    scored = [(round3(weight(s)), i) for i, s in strengths.items() if i != a_id]
    scored = [(score, i) for score, i in scored if score > 0.0]
    scored.sort(key=lambda t: (-t[0], -t[1]))
    return [i for _, i in scored]


def strengths_fts5(db, title, a_id):
    grams = trigrams(title)
    if not grams:
        return {}
    query = "{title description} : (" + " OR ".join(f'"{g}"' for g in grams) + ")"
    scores = list(db.execute(
        "SELECT rowid, bm25(search_index) FROM search_index"
        " WHERE search_index MATCH ? AND kind = 'task'", (query,)))
    own = next((s for i, s in scores if i == a_id), None)
    if own is None or own >= 0.0:
        return {}
    return {i: clamp(s / own) for i, s in scores if i != a_id}


def strengths_pg(pg, title, a_id, similarity):
    rows = pg.run(f"SELECT rowid, {similarity}({lit(title)}, {TD})::float8 FROM search_docs"
                  " WHERE kind = 'task';")
    scores = {int(r[0]): float(r[1]) for r in rows}
    own = scores.get(a_id, 0.0)
    if own <= 0.0:
        return {}
    return {i: clamp(s / own) for i, s in scores.items() if i != a_id}


def strengths_a3(rows_by_id, title, a_id):
    grams = trigrams(title)
    tasks = [i for i, r in rows_by_id.items() if r["kind"] == "task"]
    scores = bm25(rows_by_id, grams, ["title", "description"], tasks)
    own = scores.get(a_id, 0.0)
    if own >= 0.0:
        return {}
    return {i: clamp(s / own) for i, s in scores.items() if i != a_id and s < 0.0}


# --- the plan review's search candidates (reference) -----------------------

def is_unspaced(c):
    o = ord(c)
    return o == 0x3005 or 0x3040 <= o <= 0x30FF or 0x31F0 <= o <= 0x31FF or 0x3400 <= o <= 0x4DBF \
        or 0x4E00 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF or 0xFF66 <= o <= 0xFF9F


def any_word_query(text):
    """domain::search::any_word_query."""
    words = []

    def take(word):
        if len(word) >= TRIGRAM and len(words) < MAX_QUERY_WORDS \
                and not any(seen.lower() == word.lower() and seen.isascii() == word.isascii()
                            for seen in words):
            words.append(word)

    current = ""
    for c in text + " ":
        if c.isalnum() or c in "_-./":
            current += c
            continue
        run, kind = "", None
        for d in current + "\0":
            this = None if d == "\0" else is_unspaced(d)
            if run and this != kind:
                if kind:
                    for i in range(len(run) - TRIGRAM + 1):
                        gram = run[i:i + TRIGRAM]
                        if not all(0x3040 <= ord(x) <= 0x309F for x in gram):
                            take(gram)
                else:
                    take(run.strip(".-/"))
                run = ""
            if d != "\0":
                run, kind = run + d, this
        current = ""
    return " OR ".join(f'"{w}"' for w in words) if words else None


def plan_review_table(db, pg, rows_by_id, by_sha, repo):
    """Whether b is among the search candidates the plan review shows for a."""
    table = []
    with open(os.path.join(HERE, "pairs.tsv"), encoding="utf-8") as f:
        pairs = [line.split() for line in f if line.strip()]
    for why, a_short, b_short in pairs:
        a = by_sha[git(repo, "rev-parse", a_short).decode().strip()]
        b = by_sha[git(repo, "rev-parse", b_short).decode().strip()]
        query = any_word_query(a["title"])
        fts, long_, short, toks = parse_terms(query)
        a1 = search_pg(pg, toks, short, long_, pred_a1, "word_similarity", True)
        a2 = search_pg(pg, toks, short, long_, pred_a2, "bigm_similarity", True)
        scores = bm25(rows_by_id, long_, COLS, a1)
        orders = {"fts5": search_fts5(db, fts, short, True), "a1": a1, "a2": a2,
                  "a3": sorted(a1, key=lambda i: (scores[i], -i))}
        picked = {k: [i for i in v if i != a["rowid"]][:DUPLICATE_CANDIDATES]
                  for k, v in orders.items()}
        row = {"a": a_short, "b": b_short, "words": len(long_)}
        for name, top in picked.items():
            row[f"{name}_b_in_candidates"] = int(b["rowid"] in top)
            row[f"{name}_common"] = len(set(top) & set(picked["fts5"]))
        table.append(row)
    return table


PLAN_REVIEW_COLUMNS = ["a", "b", "words"] + [
    f"{n}_{c}" for n in ("fts5", "a1", "a2", "a3") for c in ("b_in_candidates", "common")]


# --- comparing -------------------------------------------------------------

def jaccard(a, b):
    a, b = set(a), set(b)
    return 1.0 if not (a | b) else len(a & b) / len(a | b)


def top_stats(base, other):
    x, y = base[:TOP], other[:TOP]
    common = len(set(x) & set(y))
    same = sum(1 for p, q in zip(x, y) if p == q)
    denominator = min(TOP, max(len(base), len(other)))
    return common, (1.0 if denominator == 0 else common / denominator), same


def f6(x):
    return "" if x is None else f"{x:.6f}"


def search_table(args, db, pg, rows_by_id):
    table, tops = [], []
    with open(os.path.join(HERE, "queries.tsv"), encoding="utf-8") as f:
        queries = [line.rstrip("\n").split("\t", 1) for line in f if line.strip()]
    for cls, query in queries:
        row = {"class": cls, "query": query}
        try:
            fts, long_, short, toks = parse_terms(query)
            if not short:
                boolean_tree(toks)  # judges the operators' placement as FTS5 does
        except Rejected as e:
            # What FTS5 itself says of the expression: a syntax error, or only
            # parse_terms' rule on short terms.
            expression = " ".join(f'"{v}"' if k == "term" else v for k, v in tokens(query))
            try:
                db.execute("SELECT count(*) FROM search_index WHERE search_index MATCH ?",
                           (expression,)).fetchone()
                fts5_said = "accepted"
            except sqlite3.Error:
                fts5_said = "rejected"
            row.update(status="rejected", reason=str(e), fts5_syntax=fts5_said)
            table.append(row)
            continue
        base = search_fts5(db, fts, short)
        a1 = search_pg(pg, toks, short, long_, pred_a1, "word_similarity")
        a2 = search_pg(pg, toks, short, long_, pred_a2, "bigm_similarity")
        if long_:
            scores = bm25(rows_by_id, long_, COLS, a1)
            a3 = sorted(a1, key=lambda i: (scores[i], -i))
        else:
            a3 = list(a1)
        row.update(status="ok", kind="short_only" if not long_ else "has_long",
                   fts5_count=len(base))
        for name, ids in (("a1", a1), ("a2", a2), ("a3", a3)):
            common, ratio, same = top_stats(base, ids)
            row.update({f"{name}_count": len(ids), f"{name}_jaccard": jaccard(base, ids),
                        f"{name}_top10_common": common, f"{name}_top10_ratio": ratio,
                        f"{name}_top10_same_pos": same})
            only_base = sorted(set(base) - set(ids))
            only_other = sorted(set(ids) - set(base))
            if only_base or only_other:
                tops.append([query, name, "only_fts5", " ".join(map(str, only_base[:20]))])
                tops.append([query, name, "only_" + name, " ".join(map(str, only_other[:20]))])
            tops.append([query, name, "top10", " ".join(map(str, ids[:TOP]))])
        tops.append([query, "fts5", "top10", " ".join(map(str, base[:TOP]))])
        table.append(row)
    return table, tops


SEARCH_COLUMNS = ["class", "query", "status", "reason", "fts5_syntax", "kind", "fts5_count"] + [
    f"{n}_{c}" for n in ("a1", "a2", "a3")
    for c in ("count", "jaccard", "top10_common", "top10_ratio", "top10_same_pos")]


def related_table(args, db, pg, rows_by_id, by_sha, repo):
    table = []
    with open(os.path.join(HERE, "pairs.tsv"), encoding="utf-8") as f:
        pairs = [line.split() for line in f if line.strip()]
    for why, a_short, b_short in pairs:
        a_sha = git(repo, "rev-parse", a_short).decode().strip()
        b_sha = git(repo, "rev-parse", b_short).decode().strip()
        a, b = by_sha[a_sha], by_sha[b_sha]
        a_id, b_id = a["rowid"], b["rowid"]
        title = a["title"]
        strengths = {"fts5": strengths_fts5(db, title, a_id),
                     "a1": strengths_pg(pg, title, a_id, "word_similarity"),
                     "a2": strengths_pg(pg, title, a_id, "bigm_similarity"),
                     "a3": strengths_a3(rows_by_id, title, a_id)}
        ranks = {k: ranking(v, a_id) for k, v in strengths.items()}
        row = {"why": why, "a": a_short, "a_subject": title, "b": b_short, "b_subject": b["title"]}
        base_top = set(ranks["fts5"][:TOP])
        for name in ("fts5", "a1", "a2", "a3"):
            order = ranks[name]
            place = order.index(b_id) + 1 if b_id in order else None
            top = set(order[:TOP])
            strength = strengths[name].get(b_id, 0.0)
            row.update({f"{name}_b_rank": place if place else "圏外",
                        f"{name}_in_top10": int(bool(place and place <= 10)),
                        f"{name}_in_top5": int(bool(place and place <= 5)),
                        f"{name}_top_size": len(top), f"{name}_b_strength": f6(strength)})
            if name != "fts5":
                base_strength = strengths["fts5"].get(b_id, 0.0)
                larger = max(len(top), len(base_top))
                common = len(top & base_top)
                row.update({
                    f"{name}_top_common": common,
                    f"{name}_top_ratio": 1.0 if larger == 0 else common / larger,
                    f"{name}_strength_diff": abs(strength - base_strength),
                    f"{name}_weight_diff": abs(weight(strength) - weight(base_strength)),
                    f"{name}_crossed": int((strength < SEARCH_FLOOR) != (base_strength < SEARCH_FLOOR)),
                })
        table.append(row)
    return table


RELATED_COLUMNS = ["why", "a", "a_subject", "b", "b_subject"] + [
    f"fts5_{c}" for c in ("b_rank", "in_top10", "in_top5", "top_size", "b_strength")] + [
    f"{n}_{c}" for n in ("a1", "a2", "a3")
    for c in ("b_rank", "in_top10", "in_top5", "top_size", "top_common", "top_ratio",
              "b_strength", "strength_diff", "weight_diff", "crossed")]


def criteria(search, related):
    out = []
    ok = [r for r in search if r["status"] == "ok"]
    for name in ("a1", "a2", "a3"):
        differ = [r["query"] for r in ok if r[f"{name}_jaccard"] != 1.0]
        short = [r for r in ok if r["kind"] == "short_only"]
        short_bad = [r["query"] for r in short
                     if r[f"{name}_top10_same_pos"] != min(TOP, r["fts5_count"])]
        long_ = [r for r in ok if r["kind"] == "has_long"]
        ratios = [r[f"{name}_top10_ratio"] for r in long_]
        low = [r["query"] for r in long_ if r[f"{name}_top10_ratio"] < 0.6]
        dropped = [f'{r["a"]}/{r["b"]}' for r in related
                   if r["fts5_in_top10"] and not r[f"{name}_in_top10"]]
        top_ratio = [r[f"{name}_top_ratio"] for r in related]
        sd = [r[f"{name}_strength_diff"] for r in related]
        wd = [r[f"{name}_weight_diff"] for r in related]
        mean_c, mean_e = sum(ratios) / len(ratios), sum(top_ratio) / len(top_ratio)
        passed = {"a": len(differ) <= 2, "b": not short_bad,
                  "c": mean_c >= 0.8 and not low, "d": len(dropped) <= 1, "e": mean_e >= 0.7}
        out.append({
            "adapter": name,
            "a_jaccard_not_1": len(differ), "a_queries": " | ".join(differ), "a_pass": passed["a"],
            "b_short_only": len(short), "b_not_same": len(short_bad), "b_queries": " | ".join(short_bad),
            "b_pass": passed["b"],
            "c_has_long": len(long_), "c_mean_ratio": f6(mean_c), "c_below_06": len(low),
            "c_queries": " | ".join(low), "c_pass": passed["c"],
            "d_fts5_in_top10": sum(r["fts5_in_top10"] for r in related), "d_dropped": len(dropped),
            "d_pairs": " | ".join(dropped), "d_pass": passed["d"],
            "e_mean_ratio": f6(mean_e), "e_pass": passed["e"],
            "strength_diff_max": f6(max(sd)), "strength_diff_mean": f6(sum(sd) / len(sd)),
            "weight_diff_max": f6(max(wd)), "weight_diff_mean": f6(sum(wd) / len(wd)),
            "crossed_floor": sum(r[f"{name}_crossed"] for r in related),
            "b_in_top5": sum(r[f"{name}_in_top5"] for r in related),
            "b_in_top10": sum(r[f"{name}_in_top10"] for r in related),
        })
    return out


def write_csv(path, columns, rows):
    with open(path, "w", encoding="utf-8", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(columns)
        for row in rows:
            writer.writerow([fmt(row.get(c, "")) for c in columns])


def fmt(v):
    return f"{v:.6f}" if isinstance(v, float) else v


def self_check():
    """The examples of the Rust unit tests of trigram_query and of Rust's rounding."""
    assert trigrams("ab abab: ..") == ["aba", "bab", "ab:"]
    assert trigrams("a b") == []
    assert any_word_query("重複した計画の候補を一覧にする") is not None
    assert any_word_query("Foo foo docs/adr. の") == '"Foo" OR "docs/adr"'
    assert any_word_query("にする") is None
    assert trigrams("(a*b) ^cd") == ["(a*", "a*b", "*b)", "^cd"]
    assert round3(0.0005) == 0.001 and round3(0.4) == 0.4
    assert parse_terms("着地 AND worktree")[:3] == ('"worktree"', ["worktree"], ["着地"])
    assert render(boolean_tree(tokens("a1b NOT c1d e1f OR g1h")), lambda t: t) == \
        "((a1b AND NOT (c1d AND e1f)) OR g1h)"


def main():
    self_check()
    p = argparse.ArgumentParser()
    p.add_argument("--repo", required=True)
    p.add_argument("--commit", required=True)
    p.add_argument("--psql", required=True)
    p.add_argument("--out", required=True)
    args = p.parse_args()
    os.makedirs(args.out, exist_ok=True)
    started = time.monotonic()
    rows, by_sha, n_commits, n_adrs = corpus(args.repo, args.commit)
    rows_by_id = {r["rowid"]: r for r in rows}
    db = load_sqlite(rows)
    pg = Psql(args.psql)
    load_pg(pg, rows)
    loaded = time.monotonic()
    search, tops = search_table(args, db, pg, rows_by_id)
    searched = time.monotonic()
    related = related_table(args, db, pg, rows_by_id, by_sha, args.repo)
    done = time.monotonic()
    plan_review = plan_review_table(db, pg, rows_by_id, by_sha, args.repo)
    reviewed = time.monotonic()
    write_csv(os.path.join(args.out, "search.csv"), SEARCH_COLUMNS, search)
    write_csv(os.path.join(args.out, "related.csv"), RELATED_COLUMNS, related)
    write_csv(os.path.join(args.out, "plan_review.csv"), PLAN_REVIEW_COLUMNS, plan_review)
    with open(os.path.join(args.out, "search_top.csv"), "w", encoding="utf-8", newline="") as f:
        csv.writer(f).writerows([["query", "adapter", "what", "rowids"]] + tops)
    crit = criteria(search, related)
    write_csv(os.path.join(args.out, "criteria.csv"), list(crit[0]), crit)
    meta = pg.run("SELECT version(), (SELECT extversion FROM pg_extension WHERE extname='pg_trgm'),"
                  " (SELECT extversion FROM pg_extension WHERE extname='pg_bigm'),"
                  " datlocprovider, datcollate, datctype, datlocale, pg_encoding_to_char(encoding)"
                  " FROM pg_database WHERE datname = current_database();")[0]
    sample = pg.run("SELECT show_trgm('着地の検証'), show_bigm('着地の検証'),"
                    " word_similarity('着地', '着地の検証'), bigm_similarity('着地', '着地の検証'),"
                    " '着地の検証' ILIKE '%着地%';")[0]
    with open(os.path.join(args.out, "meta.csv"), "w", encoding="utf-8", newline="") as f:
        w = csv.writer(f)
        w.writerow(["key", "value"])
        w.writerow(["commit", args.commit])
        w.writerow(["commits", n_commits])
        w.writerow(["adrs", n_adrs])
        w.writerow(["sqlite", sqlite3.sqlite_version])
        for k, v in zip(["postgres", "pg_trgm", "pg_bigm", "provider", "lc_collate", "lc_ctype",
                         "datlocale", "encoding"], meta):
            w.writerow([k, v])
        for k, v in zip(["show_trgm", "show_bigm", "word_similarity_sample",
                         "bigm_similarity_sample", "ilike_sample"], sample):
            w.writerow([k, v])
    with open(os.path.join(args.out, "timings.txt"), "w", encoding="utf-8") as f:
        f.write(f"load {loaded - started:.1f}s\nsearch {searched - loaded:.1f}s\n"
                f"related {done - searched:.1f}s\nplan_review {reviewed - done:.1f}s\n")
    print(f"{len(search)} queries, {len(related)} pairs, {done - started:.0f}s", file=sys.stderr)


if __name__ == "__main__":
    main()
