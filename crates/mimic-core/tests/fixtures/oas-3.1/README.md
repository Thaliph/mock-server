# OpenAPI 3.1 JSON Schemas (test fixtures)

The official OpenAPI Initiative JSON Schemas for OpenAPI 3.1, used by the `mimic-core` tests
(spec section 12.7, UI-01) to validate every generated document offline. They are **unmodified**
copies; they are test data only and are not compiled into any binary.

- Source repository: <https://github.com/OAI/spec.openapis.org>
- Commit: `99da7018fa1a7768a9c5873e072f7fbc5924a7d3` (default branch `main`, taken on 2026-11-02)
- Licence: Apache-2.0, copy in `LICENSE` (the repository's `LICENSE` at that commit)

| File | Path in the repository (published URL under `https://spec.openapis.org/`) | SHA-256 |
|---|---|---|
| `schema-base.json` | `oas/3.1/schema-base/2022-10-07` | `14cd42cfaba6b4700b5995e60b6e0bf8ed9d800f996b00d22fa91fad21b4442c` |
| `schema.json` | `oas/3.1/schema/2022-10-07` | `da01ba28852cac0de53893797cb8d1942bc3b05084f526dcc216717dec314ed0` |
| `dialect-base.json` | `oas/3.1/dialect/base` | `8a0e89e365dadbebce2921ce6244340c1090e9d544c60d977e9ad6b97a61227b` |
| `meta-base.json` | `oas/3.1/meta/base` | `267a88226e64e96dfc8c89dbd7e863160c84715e0fb893ca1d9fbf9f830f1f54` |
| `LICENSE` | `LICENSE` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |

To refresh them, download the same paths at a newer commit, compare, and update this table.
