#!/usr/bin/env python3
"""Public microdot vectors: the same reader paths as the private ones (a v3 dot read byte-exact; a semagram of three
components read whole), made from synthetic text with a fixed test seed, so the public tree can test them.
Deterministic: run it again and the bytes are the same. Usage: python3 make_public_vectors.py [out_dir]"""
import hashlib, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mdot

SEED = hashlib.sha256(b'oracle-public-vectors-2026-11-01').digest()
WHEN = 1_700_000_000_000


def main(out):
    os.makedirs(out, exist_ok=True)
    one = mdot.pack(SEED, b'# a public test dot\nOne line of synthetic text, signed by a test key.\n', WHEN, ver=3)
    mdot.write_face(os.path.join(out, 'public-v3.png'), one)
    a = mdot.pack(SEED, b'# root\nA synthetic idea.\n', WHEN + 1)
    b = mdot.pack(SEED, b'# part\nA part of it.\n', WHEN + 2, prev=hashlib.sha256(a).digest())
    c = mdot.pack(SEED, b'# same\nThe same thing, said again.\n', WHEN + 3, prev=hashlib.sha256(b).digest())
    comp = mdot.pack(SEED, mdot.layout([('root', a), ('part-of', b), ('is-same-as', c)], '# three public dots'),
                     WHEN + 4, prev=hashlib.sha256(c).digest(), ver=4, flags=1)
    mdot.write_semagram(os.path.join(out, 'public-semagram.png'), comp, [a, b, c])
    for f in ('public-v3.png', 'public-semagram.png'):
        r = mdot.read_image(os.path.join(out, f))
        assert r['ok'], (f, r.get('why'))
        print(f, 'dots', len(r['dots']), 'first', r['dots'][0]['dot_id'][:16], 'composite', (r.get('composite') or '')[:16], 'complete', r.get('complete'))


if __name__ == '__main__':
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vectors'))
