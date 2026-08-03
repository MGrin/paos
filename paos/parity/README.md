# Parity harnesses

## What is left, and why

`difflib_parity.py` — **still live and still worth running.** Its reference is
CPython's own `difflib`, which is not going anywhere, so it keeps proving that
`paos_memory::difflib::ratio` matches the algorithm `librarian.draft()` used to branch
on. 2,009 real fact pairs, byte-identical to 17 significant digits.

```sh
python3 parity/difflib_parity.py build /tmp/pairs.txt 2000
python3 parity/difflib_parity.py ratios /tmp/pairs.txt > /tmp/py.txt
cargo run --release --bin difflib-parity < /tmp/pairs.txt > /tmp/rs.txt
cmp /tmp/py.txt /tmp/rs.txt
```

## What was removed with the Python, and why NOT to resurrect it

The other eight compared against `memory_facet.py`, `librarian_facet.py` and
`trajectory_facet.py`. Those files are gone, so those harnesses can never pass again. A
check that cannot pass is not coverage — it is a red mark people learn to scroll past,
which is the same failure as an exclusion that outlives its reason.

The evidence they produced is in the git history where it can still be read:

| harness | what it proved | commit |
|---|---|---|
| `trajectory.sh` | 367/367 byte-identical, 120 transcripts / 63,314 JSONL lines | `paos trajectory` |
| `prompt_parity.py` | 3,830 bytes of prompt + 590,921 bytes assembled, byte-identical | the four prompts |
| `screen_parity.py` | 1,245 real texts, byte-identical incl. the quoted match | the review queue |
| `upkeep_parity.py` | 599 facts → 8 groups, identical group MEMBERSHIP | tidy and split |
| `lessons_parity.py` | 337 episodes → 154 signatures → 15 recurring, identical | lessons |
| `chunk_parity.py` | 121 sessions × 3 chunk sizes, identical | dream chunking |
| `memory_cli.sh` + `py_memory.py` | 28/28 across stdout, stderr AND exit codes | the CLI cutover |

What replaces them is unit tests that assert the BEHAVIOUR rather than agreement with a
second implementation — the split ordering, the resurrection guard, the recurrence gate,
the U+2028 separators, the forget gate. Those outlive the port; a diff against a deleted
file does not.

## The one rule worth carrying forward

Six defects were found in these harnesses during the port, and every one had the same
root: **the check was not testing what its name said it was.** Three failed safe (a
correct port looked broken); two could have inverted (a reference bent to fit the port, a
reference silently re-routed to a different binary); one simply never looked at stderr.

So: address a reference by MODULE, never by a command name that can be re-pointed; compare
stdout, stderr and exit code separately; and diff the real binaries at least once per
verb, not just the libraries.
