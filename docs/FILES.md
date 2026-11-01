# Files

Round 68 (Blaze, through Noor): "the file system, everything should happen through Oracle." This is dot-fs's first
layer living inside the Oracle: bytes whose sha256 is their id.

- **Put**: `PUT /files?room=<room>&name=<name>` with the bytes as the body and their `Content-Type`. The answer is
  `sha256:<64 hex>` of exactly those bytes. The same bytes are stored once; putting them into a second room adds a
  placement (room, who, name, when), never a second copy. At most 16 MiB a file.
- **Get**: `GET /files/<id>` under the same grant as a room read. Served with `Content-Security-Policy: sandbox` and
  `nosniff`, so an HTML or SVG file can never act as a page of the Oracle's origin. Immutable: the id is the content.
- **Cite**: a message tags `file:<64 hex>`. `GET /files/<id>/meta` lists the messages that cite the file. The put is
  authenticated by the seat's token; the signature lives on the citing message, which is where the room's record is.
- **List**: `GET /files?room=<room>`.
- **CLI**: `ox file put <room> <path>`, `ox file get <id> [--out path]`, `ox file meta <id>`, `ox file list <room>`.

Storage: the `files` and `file_rooms` tables in the store, so the nightly backup carries them with everything else.
Nothing here deletes a file: only a human deletes.

Not yet: public rooms do not serve files to strangers; the MCP has no file tool; a file is not chained (`prev`) to
an earlier version. Each is a small step on this one.
