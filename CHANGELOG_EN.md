# Changelog (English)

Russian version: [CHANGELOG.md](CHANGELOG.md).

Format — [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
Versioning — [SemVer](https://semver.org/).

## [0.39.0] — 2026-06-18

**The daemon no longer hangs on bulk git updates of 1C repos. A metadata-composition change (`Configuration.xml` in the batch) now rebuilds only the lightweight XML enrichment layer, not the heavy code layer — no reindex required (daemon serve watcher-path change).**

> Context. On a bulk update of a local BSL repo (`git reset --hard` / `pull` to a distant commit) the watcher collects a batch containing a changed `Configuration.xml`. Previously this triggered a FULL `run_index_extras` right inside the watcher loop — rebuilding `metadata_*` + `data_links` + `role_rights` + `code_usages` + `procedure_terms` (hundreds of thousands of procedures) + the call graph, in a "live" context competing with the `serve` reader for SQLite → busy-spin at 100% CPU for tens of minutes. Reproduced identically on the VM (docker) and Windows.

### Fixed

- **A `Configuration.xml` change no longer triggers a full re-enrichment in the watcher loop.** In `run_incremental_extras`, the `config_changed` branch (fires on any changed `Configuration.xml` — which is rewritten on every `DumpConfigToFiles` export, not only on real composition edits) used to `return run_index_extras(...)`, a full heavy pass. It now rebuilds only the **XML layer** and does NOT `return`: the heavy code layer is kept current by per-file `update_*_for_file` over the batch's `.bsl`. On full UT (57K files) a config-changed batch of 43 `Configuration.xml` files takes **~21 s** (XML layer) instead of a multi-minute hang. The object list stays correct — adding/removing/renaming an object is reflected in `metadata_objects` equivalently to a full rebuild (3 regression tests).

### Changed

- **`run_index_extras` split into an XML layer and a code layer.** The new `run_index_extras_metadata_layer` builds the object list, structure (`attributes_json`), data links, config-level edges, role rights, synonyms, forms, subscriptions, modules — an XML-export walk (seconds even on UT). The heavy code layer (`metadata_code_usages`, `procedure_terms`, `build_call_graph`) is called from the full `run_index_extras` afterwards, and is left untouched on the incremental path on a composition change. A full rebuild still runs on initial reindex and `index --force`. Full-pass behavior is unchanged: phase order preserved, dependencies (attributes/synonyms → metadata_objects, call_graph → forms/subscriptions) respected.

## [0.38.1] — 2026-06-17

**The daemon no longer rebuilds enrichment tables for nothing on startup. A daemon restart on unchanged data is now instant (previously every start = a full rebuild of `metadata_*`/terms/call-graph, minutes on large configs).**

### Fixed

- **Gate against idle re-enrichment on daemon startup.** On startup, after `full_reindex` (mtime fast-path), the daemon **unconditionally** ran the full `index_extras` — rebuilding `metadata_objects`/`data_links`/`role_rights`/`code_usages`/`procedure_terms` (hundreds of thousands of procedures)/`forms`/`subscriptions`, even when mtime reported "0 changes". On the full federation (UT/BP-SS/BP-TDK/ZUP) that was ~15 minutes wasted on any container restart. Now `index_extras` is skipped when: the DB already existed, `full_reindex` indexed 0 and deleted 0 files, and the processor's extras tables are non-empty (`LanguageProcessor::extras_present` — for BSL: non-empty `metadata_objects` + mechanical terms in `procedure_enrichment`). Any data change, a new DB, or empty extras → a full pass as before; incremental edits are still handled by the watcher loop via `index_extras_for_files`.
- **Limitation:** the gate does not track the extras SCHEMA. If a release adds a new extras table, it will stay empty on unchanged data — such releases need a one-off full rebuild (`index --force` or a DB rebuild). Noted in the `extras_present` doc comment.

## [0.38.0] — 2026-06-17

**Guard the output against client-side disk offload. Heavy responses (a large module's map, long arrays of values/sources/attributes) are trimmed at the source to a sample or a compact form — with a marker for the full count — and are no longer dumped to a file by the client at the cost of a lost turn. No reindex required (all on the serve output layer).**

> Context. The client (`claude` CLI / Claude Code) caps a single `tool_result` streamed inline into context (`MAX_MCP_OUTPUT_TOKENS` ≈ 25,000 tokens). A response over the cap is **dumped to a file on disk** by the harness, handing the model only a path + preview — after which structured inline access is lost and the model greps the file in extra turns. The core hard caps (`grep_*` 1 MB, `read_file` 2 MB) miss this class: it's not one giant string but a long array (enum values, subscription sources, the function map of a large module).

### Added

- **`cap_response` — a generic response-size guard (serve layer).** While the serialized JSON exceeds the `[cap].max_response_bytes` budget (default **48,000 bytes** ≈ 12–24k Cyrillic tokens, comfortably under the 25k-token offload), it repeatedly finds the heaviest array-valued key and halves it, leaving `<key>_total` (original element count, set once) and `<key>_truncated: true` next to it. Only **arrays** are trimmed — large strings (`read_file`/`grep` content) are untouched. Gated by the `[cap].cap_tools` list (default: `get_event_subscriptions`, `bsl_sql`, `find_references`, `get_register_writers`).
- **`[cap].cap_enabled` — a global on/off switch for `cap_response`** (default `true`). Takes precedence over the list: when `false`, cap applies to nothing regardless of `cap_tools` (structural omit and the navigational body cap work independently). Needed because an empty `cap_tools` means “default set”, not “off”.
- **`omit_oversize_sections` for structural tools (`get_object_structure`).** Where an array/map is the FULL authoritative answer (a 1C object's structure), a partial sample would lie (“here are all the enum values”). So the heaviest section (array > 1 element / map > 16 keys) is dropped WHOLESALE with `<section>_omitted: true` + `<section>_count: N` — a section is either fully present or honestly omitted with its count.
- **Navigational body cap (`get_function`/`get_class`).** A body longer than `[cap].max_function_body_chars` (default **15,000 chars**) → a navigational stub: head + tail + marker + a hint to `read_file(line_start,line_end)` / `grep_body`. A body is connected code, so it's cut head+tail (not by the middle) with an exact line range.
- **`[cap]` config:** `max_response_bytes`, `cap_tools`, `cap_enabled`, `max_function_body_chars` (all optional; 0 for the byte/char thresholds disables the guard).

### Fixed

- **`get_file_summary` on giant modules no longer goes to disk offload.** `get_file_summary` is a core tool wrapped via `wrap_with_meta_extra`, where `cap` doesn't apply to the core; adding it to `cap_tools` didn't cover it. On `УправлениеДоступомСлужебный` (972 functions, 47,399 lines), even the compact map (`MAP_DETAIL_CAP = 120` — names+lines only, no signatures/docstrings) was **100,164 chars** on a single line → the client dumped the response to a file and the turn was lost (weak spot #4 of the UT-11 run, Q08 “RLS”). Now the core calls `cap_response` before wrapping: a sample + `functions_total` + `functions_truncated` remain, and the response is returned inline.

### Measurement (UT-11 run, Q08 “access rights / RLS”, Sonnet)

- On the fixed build: **0** disk offloads over the run, the largest `tool_result` was **19,128 chars** (~4.8k tokens) against the ~25k-token offload threshold. 38 `*_truncated` markers in the trace = the cap mechanisms working as intended. Verdict on the question — COMPLETE.
- Smoke via federation (production UT, `УправлениеДоступомСлужебный` 47,399 lines): `functions_total: 972`, `functions_truncated: true`, compact map — response inline (~48 KB), offload eliminated.

## [0.37.1] — 2026-06-16

**Deterministic counts in BSL tools + compact subscription output when filtered by `source`. The model cites a ready-made number instead of recounting an array (LLMs undercount long lists), and `get_event_subscriptions` with a filter no longer blows past the output limit. No reindex required.**

### Added

- **`get_register_writers`: counts `writers_count` / `writes_to_count` + `*_count_by_type`** (per register type). The model used to recount the array of names by hand and undercount (43 → “40”); now the number is ready in the response.
- **`get_object_structure`: a `counts` section** — element count for each array section (`tabular_sections`, `attributes`, `dimensions`, `resources`, `enum_values`…). Fixes the tabular-section undercount (10 → “5”).

### Changed

- **`get_event_subscriptions` with a `source` filter: instead of the full `sources` array — `sources_count` + `matches_source: true`.** For global events (`ПередЗаписью` etc.) subscriptions carry `sources` of up to hundreds of types (one had 256); echoing them ballooned the response past 80K chars and broke the output limit (the response went to a file, a turn was lost). Now for `source=РеализацияТоваровУслуг`: 80,183 → ~2,700 chars. Without a filter — the full `sources`, as before.

## [0.37.0] — 2026-06-16

**Output token economy + robust resolution of 1C object names in BSL tools. Compact output format for `grep_*`/`list_files`; single-object BSL tools accept Russian type prefixes and no longer depend on the argument key name; `find_symbol` tolerates name synonyms. Output-format and resolution changes — NO reindex required.**

### Changed

- **Compact output format for `grep_body`/`grep_code`/`grep_text`/`list_files` (core).** Instead of JSON objects with repeated keys (`{"line":N,"content":"X"}` on each of thousands of matches) — flat strings grouped by file: `grep_text`/`grep_code` → `"N: content"`; `grep_body` → `"<name> (<kind>) L<start>-<end>: <lines>(+N)"`; `list_files` → `"<path> | <lang> | <N> lines | <size>"`. The structural JSON overhead was the main token cost (~55% of the response). `_meta` (dependent_files/file_mtimes) is assembled separately and not duplicated. Default `limit` lowered 100 → 30.
- **`find_symbol`: sharpened description.** Call by bare name only for a unique name; for standard handlers / common names — pass `path_glob` right away (otherwise hundreds of locations, `truncated`).

### Fixed

- **BSL tools accept Russian type prefixes.** `get_object_structure`/`get_register_writers`/`get_data_links`/`find_data_path`/`find_references`/`get_object_profile`/`get_event_subscriptions`/`get_form_handlers`/`bsl_sql` with `object="Документ.X"` used to return empty (the index stores English types only). Input normalization via the `META_FORMS` table (`canonical_meta_type`/`normalize_object_ref`/`normalize_sql_object_refs`): `Документ.X` → `Document.X`; for `bsl_sql` — both singular and plural forms in query literals. Eliminates a wasted cascade: an empty `get_register_writers` triggered an avalanche of `read_file` over ManagerModule.
- **Single-object BSL tools no longer depend on the key name.** `get_register_writers`/`get_data_links`/`find_references`/`get_object_profile`/`get_object_structure` take the value of the first non-empty string argument, skipping service keys (`repo`/`depth`/`limit`/…) — the model no longer trips on `object` vs `full_name`.
- **`find_symbol` accepts `symbol`/`query` as aliases of `name` (core).** A call with `symbol=…` previously failed with the opaque deserializer error “missing field `name`” (a wasted turn). `#[serde(alias)]` — synonyms are picked up as `name`; the schema contract (required `name`) is unchanged.

### Benchmark (UT 11.5, question “lifecycle of РеализацияТоваровУслуг”)

- Old format (baseline): **1,282,904** tokens / 33 turns.
- Compact format WITHOUT name resolution: **2,172,388** / 48 (+69% — empty `get_register_writers` drove a `read_file` cascade).
- Compact format + name resolution (final): **917,247** / 24, clean re-run **926,170** / 27, `retry=0`, verdict COMPLETE — vs baseline **−28%**.

## [0.36.0] — 2026-06-14

**CORE B: the call qualifier is preserved in the graph. BSL stores `callee` glued (`Module.Method`) — consistent with the other languages; precise resolution of common-module and manager-module calls. Direct-edge resolution 52% → ~80-82%, zero false bindings.**

> ⚠️ **Full reindex required** (`index --force` per repo): the format of `calls.callee` (BSL) and `proc_call_graph` changed. The mtime fast-path won't pick it up. On federation — rebuild all nodes synchronously.

### Changed

- **CORE (engine, affects all languages): BSL no longer drops the call qualifier.** When parsing `Module.Method()` the engine previously stored only the bare method name in `calls.callee` — the qualifier `Module` lives in a sibling node of the onescript tree and was ignored, making BSL the only language that lost the receiver. BSL now glues `receiver.method` the same way Python/JS/Go/… already store `obj.method`, so the call graph is uniform across languages. Bare local calls stay a bare name. **Affects `get_callers`/`get_callees`/`find_path`/`get_call_tree` and `find_path_bsl` output for BSL: qualified calls are now shown as `Module.Method`** (like Python's `requests.get`) — more informative, but a format change for BSL queries. Non-BSL languages are untouched (verified by an A/B run on 6 languages Python/JS/TS/Java/Go/Rust — `calls` output is byte-for-byte identical).

### Added

- **Common-module call resolution by qualifier (Tier C).** `ОбщегоНазначения.ЗначениеРеквизитаОбъекта` → the exact address `…/CommonModules/ОбщегоНазначения/Ext/Module.bsl::ЗначениеРеквизитаОбъекта`. Removes the dependency on the "unique export" heuristic (v0.35.0): names exported in ≥2 common modules now resolve precisely via the qualifier. On full UT: 88.3% of common-module calls bound, zero false.
- **Manager-module call resolution by chain (Tier D).** `Справочники.Номенклатура.НайтиПоКоду` → `…/Catalogs/Номенклатура/Ext/ManagerModule.bsl::НайтиПоКоду`. Collection→dump-folder mapping (`Справочники`→`Catalogs`, irregular plurals handled) from the single `META_FORMS` table. Platform manager methods (`ПустаяСсылка`, `НайтиПоКоду`) not exported in the object module correctly stay NULL. On UT: ~28k manager calls bound.
- **Object-call pruning by qualifier.** Glued `Объект.Метод` where the qualifier is a local variable / platform object (`Запрос.Выполнить`, `Выборка.Следующий`, `НаборЗаписей.Записать`) are removed from the graph more precisely than the static ballast list: knowing the receiver isn't a module, even colliding method names are cut. Three guards against losing real edges: common modules, metadata collections (`Справочники`/`Документы`/…) and multi-dot manager chains are spared; only unresolved (`callee_proc_key IS NULL`) edges are cut. Object noise is cleaned — `get_callees`/`get_call_tree` are readable (previously drowned in `Запрос.Выполнить` leaves).

### Resolution summary

- Full UT (57k files): direct-edge resolution **52.1% → 82.1%**, **zero false bindings**. Federation (UT/BP-SS/BP-TDK/ZUP): **80-82%**.

### Tests

- BSL: `resolves_callee_key_by_module_qualifier` (Tier C — collision resolved by qualifier), `prunes_object_calls_protects_modules_collections_chains` (Tier D + prune — spares modules/collections/chains, resolves manager), updated `test_parse_bsl_calls` (gluing). 150 BSL + 277 CORE green. Multi-language A/B (Python/JS/TS/Java/Go/Rust): non-BSL graph byte-for-byte identical to the old binary.

## [0.35.0] — 2026-06-14

**BSL call-graph fix: same-named procedures are split by module, call targets resolve to an address, platform ballast is pruned. CORE: call-graph edges carry the source file path.**

> ⚠️ **Full reindex required** (`index --force` per repo): the data format of `proc_call_graph` and the `calls` query output changed. The mtime fast-path won't pick it up — a force reindex is needed. On federation — rebuild all nodes synchronously.

### Fixed

- **The call graph no longer collapses same-named procedures.** `caller_proc_key` in `proc_call_graph` is now `<rel_path>::<name>` (same as `procedure_enrichment.proc_key`) instead of a bare name — built via `JOIN calls ⋈ files`. On the full UT config, 240 modules each defining their own `ОбработкаПроведения` stopped collapsing into 2 rows → 259 distinct callers. Previously `find_path_bsl`/`bsl_sql` over the graph couldn't tell the documents apart.

### Added

- **`callee_proc_key`: call-target address resolver (stage 4e).** Two safe tiers: (a) local call — a bare callee name declared in the caller's own file → `<file>::<callee>`; (b) unique export — the callee name is exported in exactly one place in the configuration (detected by `Экспорт` in the signature) → that address. The core strips the module qualifier when parsing a call (`Module.Method` → `Method`), but target uniqueness removes the ambiguity. Ambiguous / dynamic (`Object.Method` via a variable) / platform → `NULL` (a false binding is worse than an honest NULL). On UT, 52% of direct edges resolve.
- **Platform ballast pruning.** Edges into collection/object methods and platform global functions (`Вставить`/`Добавить`/`НСтр`/`Структура`…, whose target is outside configuration code) are removed from the graph (~35%; on UT 1.14M inserted → 739K). Two guards against losing real edges: only unresolved edges are removed (`callee_proc_key IS NULL`); a name that is exported anywhere in the configuration (`Записать`/`Получить`/`Удалить`…) is left untouched entirely (adaptive per UT/BP/ZUP — each computes its own export set).
- **CORE: `get_callers`/`get_callees`/`find_path`/`get_call_tree` return the source file path of each edge** (`path`, resolved `file_id → files.path`). This distinguishes same-named functions from different files — previously the output showed a bare name + numeric `file_id` ("N indistinguishable rows"). Language-neutral, no reindex required (query layer only).
- **`find_path_bsl`: walk by resolved address.** Between hops the link is `COALESCE(callee_proc_key, callee_proc_name)` — by the target address where present, otherwise by the raw name (unresolved leaf / synthetic edges). `from`/`to` accept `<rel_path>::<name>` (a bare name is allowed for unresolved leaves). Path edges now include `callee_key`.

### Tests

- BSL: `resolves_callee_keys_local_unique_export_and_null` (both tiers + honest NULL), `prunes_platform_balast_keeps_real_and_resolved` (pruning + IS NULL guard + collision guard on an exported name), `incremental_direct_shared_edge_survives` rewritten for path semantics. CORE: 277 tests green (including `get_call_tree` with `file_id` in the CTE).
- Reindex impact measurement (honest Rust A/B on full UT, 57K files): total time 54.15s → 54.86s (+0.7s, noise), call-graph build phase 18.35s → 27.18s (+8.8s). rag-query card #1524.

## [0.34.1] — 2026-06-13

**Diagnostic message when the daemon is unreachable + fix of incorrect `CODE_INDEX_HOME` docs (issue #1).**

### Fixed

- **The "daemon not running / runtime-info missing" message now explains the real cause.** The `serve` process and the daemon find each other only through `$CODE_INDEX_HOME/daemon.json`. If `serve` has the variable unset or pointing at a different folder than the daemon, runtime-info is not read — and tools reported "daemon not running" while the daemon was alive. The message now states the expected `daemon.json` path, the current `CODE_INDEX_HOME` value, and the common cause: on Linux/macOS, GUI MCP clients (VS Code, Continue, Cline) **do not read `~/.bashrc`/`~/.zshrc`**, so a `serve` they launch with an empty `env` never sees the `CODE_INDEX_HOME` from your shell, while the daemon started from a terminal does. Fix — set `CODE_INDEX_HOME` to the same absolute path in the client's MCP config `env` section. Affects `client::base_url` (all data tools and `daemon status`) and the `health` MCP tool. Reproduced on a clean Linux box with the release binary. Issue #1 (reported by @NorfLoud).
- **README/README_RU: removed an incorrect fallback claim.** The docs promised that with `CODE_INDEX_HOME` unset the daemon falls back to `%APPDATA%`/XDG — no such fallback exists in the code; the variable is required. Replaced with the correct statement + added a Troubleshooting section about `CODE_INDEX_HOME` mismatch.

## [0.34.0] — 2026-06-12

**Automatic terms fallback in `bsl_sql`: an empty result over procedure tables now returns `search_terms` output right away, not just a hint.**

### Added

- **`bsl_sql`: `terms_fallback` field on empty results over procedure tables** (`functions` / `proc_call_graph` / `procedure_enrichment`). Models ignore the v0.33 hints (5 live runs — 0 `search_terms` calls), so on `row_count == 0` the terms search now runs automatically inside the same call: words are taken from SQL string literals (`'%ПоШтрихкоду%'` → "по штрихкоду") and text `params`, normalized the same way as terms (`split_identifier`: CamelCase split, lowercase, ё→е, words ≥3 chars), then the same trigram FTS query as `search_terms` (OR over words, LIMIT 10). Response: `terms_fallback = {fts_query, results: [{proc_key, signature, score}]}`. The model uses data "in hand" as a regular result — live run test03: an empty `IN ('ХранилищеОбщихНастроек', …)` → fallback returned `ОбщегоНазначенияВызовСервера::ХранилищеСистемныхНастроекСохранить/Загрузить/Удалить`, and the model put them into the report as fact (the hint in the same run was ignored again).
- **Trigger boundary:** only BSL repos with populated enrichment tables (no terms / old index → silently no fallback, previous behavior); queries not touching procedure tables (e.g. `metadata_objects`) — previous hint. When the fallback fires, no hint is added — the structure is self-documenting; when it doesn't, the v0.33 hints stay as they were. On exam-style questions (196, rerun 2026-06-12) the fallback is neutral — it never fired; its niche is searching code by meaning.
- **Known limitation:** the `signature` field in `results` is the enrichment mechanism fingerprint (`mech:v1`), not the procedure signature.

### Tests

- Unit tests: `sql_string_literals` (escaped `''`, wildcards), `searched_proc_tables`, `terms_fallback_for_sql` (hits via literals and via text params; `None` without terms / without words ≥3 chars).

## [0.33.0] — 2026-06-11

**Empty procedure search on a BSL repo now hints at search_terms — for search_function, grep_body, grep_code, find_symbol.**

### Added

- **An empty procedure-search result on a BSL repo now hints at `search_terms`** — across `search_function`, `grep_body`, `grep_code` and `find_symbol`. Per-call analysis of the 11.06 benchmark (10 business tasks on UT-11) showed the model reflexively picks exact search (by name or text) for "find a procedure by meaning" and never reaches `search_terms` (0 calls per run), while the residual empty calls are exactly its niche (handlers living in common modules: prefixing, default values, exchange conflicts — grep over the object's own modules returns 0 because the code lives in an SSL module wired via an event subscription). A live test03 run showed three consecutive empty `grep_body` calls (steps 17/20/21), none of which pointed to `search_terms` — so the hint is attached to EVERY empty response of these tools, not just `search_function`. Same trick that worked in 0.31 (hints break chains of blind retries). Non-BSL repos keep the old hints; `search_class`/`grep_text` were left untouched (terms index procedures, not classes or xml/text).
- **An empty `bsl_sql` result set (0 rows) hints too.** A live run showed Opus reflexively prefers `bsl_sql` (LIKE over `metadata_objects`/`functions`) for "find a procedure by meaning" rather than search_function/grep, and never switches to search_terms. So on `row_count == 0`: if the query touched procedure tables (`functions`/`proc_call_graph`/`procedure_enrichment`) the hint points to `search_terms`; otherwise a generic hint (check filters; Cyrillic in `LIKE`/`=` is case-sensitive — SQLite `lower()` doesn't fold it). This covers the exact point where the model got stuck (test03 steps 10/14 — empty `bsl_sql` over `functions` with no further direction).
- **Known limitation:** the hint fires on an EMPTY result. If exact search returns a non-empty but irrelevant "noise" set (e.g. `search_function` on a frequent word yields dozens of namesake matches) the hint does not appear, since the tool cannot judge relevance (the model does). This case is partially covered by mentioning `search_terms` in the tools' descriptions.
- **Effect note:** across several live runs on Opus (test03/test05) the model never went to `search_terms` — it solved the task via `bsl_sql` over object synonyms (the same semantic bridge the terms provide). The hints are correct and harmless, may help other models/strategies, but were not a "silver bullet" on Opus — no behavioral effect was observed.

## [0.32.0] — 2026-06-11

**New object-structure sections (owners/value_types/properties/enum_synonyms/commands + attribute synonym/required), owner and functional_option_content edges in data_links, roles in metadata_objects, `{a,b}` brace alternates in path_glob, LIMIT hint in bsl_sql. Fixed "—" types for DefinedType.**

### Added

- **`get_object_structure`: five new structure sections** (driven by the 747-question 1C:Professional run on UT-11):
  - `owners` — owners of a subordinate catalog from `<Owners>` (`Catalog.ЭквайринговыеТерминалы` → `Catalog.ДоговорыЭквайринга`);
  - `value_types` — value type of a chart of characteristic types / constant from the root `<Type>`: for a CCT this is the list of available analytics dimensions (`СтатьиДоходов` → 8 types);
  - `properties` — whitelisted scalar header properties: information-register periodicity/write mode (`ЦеныНоменклатуры25` → `Periodicity=Second`), accumulation register kind, document numbering, catalog hierarchy/code lengths;
  - `enum_synonyms` — UI labels of enum values as a separate map, the `enum_values` format is unchanged (`ЗакупкаПоИмпорту` → "Импорт"; 814 labels on `ХозяйственныеОперации`);
  - `commands` — object commands `[{name, synonym?}]` from `<ChildObjects>/<Command>`: "create on basis", print forms, etc.
- **Attributes in `attributes_json` now carry `synonym` (UI label, ru-priority) and `required`** (`<FillChecking>ShowError`): "which field is mandatory in X" is now answerable without XML.
- **`data_links`: two new edge kinds** — `owner` (subordinate catalog → its owner) and `functional_option_content` (functional option → objects in its `<Content>`; `ИспользоватьЛимитыРасходаДенежныхСредств` → 3 objects).
- **Roles in `metadata_objects`**: `Role` added to the known metadata types — 1288 UT-11 roles with synonyms are reachable via `bsl_sql`/synonym search.
- **`{a,b}` brace alternates in `path_glob`/`pattern`** (`grep_code`/`grep_text`/`grep_body`/`list_files`): SQLite GLOB has no alternation, so `**/*.{bsl,xml}` silently returned nothing — the pattern is now expanded into an OR group of GLOB conditions (`expand_glob_braces`, up to 64 variants, no nesting — same as globset).
- **`bsl_sql`: a hint when the row count equals the LIMIT from the query text** — "the output may be cut by your SQL LIMIT" (previously the agent took a truncated result for a complete one).

### Fixed

- **DefinedType attribute types were reported as "—"** in object structure: a `DefinedType` is serialized in the export as `<v8:TypeSet>`, while the parser only matched `:Type` tags. Now `ИНН` → `ОпределяемыйТип.ИНН`.

### Tests

- Units: parsing of owners/TypeSet/properties/FillChecking/synonyms/value_types/enum_synonyms/commands, `expand_glob_braces` (cartesian product, nesting/unclosed brace — literal), `sql_limit_value`. Full workspace green.
- Smoke on live UT-11 (ut-test, 57,102 files): 11/11 checks green; full force reindex — 2 min 23 s (stop the services during reindex — SQLite contention slows it down by an order of magnitude).

## [0.31.0] — 2026-06-11

**Fixed the "blind" `get_form_handlers`, `source` filter and unknown-parameter rejection in `get_event_subscriptions`, hints on empty responses of graph and file tools.**

### Fixed

- **`get_form_handlers` could not find ANY form on production configurations.** The tool matched `owner_full_name = 'Document.X'` exactly (as its own docs suggested), while the DB stores values in export-folder format — `'Documents.X'` (plural; on UT-11: 1350 rows plural, 0 singular). Both formats are now accepted: exact match first, then retry with `<Singular>.<Name>` → `<PluralFolder>.<Name>` conversion (shared `meta_type_to_folder` helper, extracted from `get_object_profile`); the response echoes the actually matched DB key.
- **Broken-regex error text in `grep_body`/`grep_code`** read as "Invalid parameter name: regex parse error…" (an artifact of mapping the compile error into `rusqlite::Error::InvalidParameterName`) and misled the agent into hunting for a "wrong parameter name". Now `UserFunctionError`: "grep_body: regex parse error…".

### Added

- **`get_event_subscriptions`: `source` filter** — subscriptions by source object. Accepts `'Document.ЗаказКлиента'`, `'DocumentObject.ЗаказКлиента'` or the short name `'ЗаказКлиента'`; case-insensitive; type `Document` automatically matches `DocumentObject` from `sources_json`.
- **`get_event_subscriptions`: unknown parameters are rejected** with the list of valid filters. Previously `object=…` was silently ignored and the tool dumped ALL subscriptions (~52K tokens into the agent context instead of pointing out the mistake).
- **Smart `get_form_handlers` error**: form not found but owner exists → response carries `available_forms` (the owner's real forms); owner missing → hint about the owner format and how to verify via `get_object_structure`/`bsl_sql`.
- **Hints on empty responses** (previously a bare `{"result":[]}` — the model kept repeating the same call): `get_callers`/`get_callees` (the name must be exact, no parentheses or owner; empty also means genuinely no calls), `list_files` (pattern is a glob from the repo root), `get_imports` (file_id: no import statements — normal for BSL; module: it is the NAME of the imported module, not a file path).

### Changed

- **`get_event_subscriptions`: default limit 200 → 50.** A filterless call on UT-11 returned ~52K tokens; `truncated`+`total` in the response suggest narrowing the filter or requesting a larger limit. MAX_LIMIT (2000) unchanged.
- The empty-result hint of `get_function`/`get_class` now mentions `search_class` too (previously only `search_function`).
- `bsl_sql` description: documented the `metadata_forms.owner_full_name` format = `'<PluralFolder>.<Name>'` (`'Documents.ЗаказКлиента'`) — same convention as `metadata_modules.object_name`.

## [0.30.0] — 2026-06-11

**Mechanical term enrichment at index time (no LLM) + trigram FTS: `search_terms` works on production-size configurations for the first time. Smart `bsl_sql` errors.**

### Added

- **Mechanical filling of `procedure_enrichment.terms` at index time** — new `terms.rs` module and the `index_procedure_terms` pass (+ per-file incremental). Terms for every procedure are built from four cheap sources: words of the procedure name (CamelCase/underscore/script-change split: УточнитьДанныеПоШтрихкоду → "уточнить данные по штрихкоду"), words of the owner object name (from the module path), the owner object's SYNONYM from `metadata_objects.synonym` (a mechanical bridge between the Russian presentation and the English identifier), and the comment above the procedure. No LLM: 259,414 procedures of UT-11 get enriched within a fraction of the 63-second full rebuild. Signature `mech:v1`; rows written by the LLM `enrich` pass (different signature) are not overwritten. Ё is folded to Е at write time.
- **Trigram FTS tokenizer for terms** (`tokenize='trigram'` instead of unicode61) — word forms and substrings of 3+ characters work ("штрихкод" matches "ПоШтрихкоду"), case and ё/е are irrelevant. Existing databases migrate automatically (`ensure_trigram_tokenizer`: drop + rebuild from the content table on DDL mismatch).
- **Smart `bsl_sql` errors** — on `no such column/table` the response is extended with `did_you_mean` (Levenshtein), `column_exists_in_tables` (which tables actually contain the column — catches the meta_type/module_type confusion) and the columns of the tables referenced by the query. The error becomes self-correcting within the same turn (benchmark finding: a bare error cost the agent an extra reconnaissance turn).

### Changed

- **`search_terms` reworked for mechanical terms**: a multi-word query without explicit operators is rewritten server-side into an OR of words (implicit AND on short terms almost always returned 0 — a benchmark finding), words shorter than 3 characters are dropped, ё is folded to е, and the rewritten query is visible in the response (`fts_query`). New description ("FIRST choice for finding functionality", how to query), two contextual hints on an empty result.
- **Fixed the repo filter in `search_terms`** — the routing alias was bound instead of `'default'` (as in all other BSL tools): the tool returned nothing even with a populated table. The bug stayed invisible while the table was empty.
- **Missing-parameter error texts** of `get_function`/`get_class`/`get_object_structure` no longer suggest `names`/`full_names`: with mass-mode disabled the hint led the model into a rejected call (benchmark observation).

### Tests

- Units for `terms.rs` (Cyrillic/Latin/acronym/digit splitting, ё→е, object from path, comments), `index_procedure_terms` (mechanics + LLM protection + incremental with cleanup), `ensure_trigram_tokenizer` (migration + substring matches), OR-rewrite and ё in `search_terms` (integration), `enrich_prepare_error` (column and table cases). Full workspace green.
- E2E benchmarks on ut-test (10 business tasks, Opus headless): the "where is feature X" spiral case is solved by a single `search_terms` call instead of ~22 lexical attempts; the no-`bsl_sql` arm confirmed its value (+33% turns, +19% cost without it).

## [0.29.0] — 2026-06-10

**Synonyms for ALL metadata objects; narrow `sections` selection in `get_object_structure`; columnar `bsl_sql` result format.**

### Added

- **Synonyms (Russian presentations) for ALL metadata objects.** A new lightweight indexing pass `index_object_synonyms` fills `metadata_objects.synonym` for every object type — including `CommonModule`/`Constant`/`CommonPicture`/`FunctionalOption` and other types without an attribute structure that are not part of `OBJECT_FOLDERS` (previously only objects with a structure had a synonym). The `parse_object_header_xml` parser reads only the root XML header (meta_type/Name/Synonym) and stops at `<ChildObjects>` — the pass is cheap. `v8:lang=ru` takes priority, otherwise the first non-empty presentation; the base configuration's synonym is not overwritten by an extension. Why: the synonym is a mechanical bridge "Russian presentation ↔ English identifier" for meaning-based search without LLM enrichment.
- **`sections` parameter in `get_object_structure`** — narrow selection of structure sections (like `sections` in `get_object_profile`): return ONLY the requested keys out of `attributes`/`tabular_sections`/`dimensions`/`resources`/`posting`/`enum_values`/`predefined`. Without the parameter — all sections (backward compatible). Works in both single (`full_name`) and mass (`full_names`) mode. A context-economy lever: `["posting"]` — posting behavior at ~0.2 KB instead of the full object; `["attributes"]` — header attributes without tabular sections; `["dimensions","resources"]` — for registers.

### Changed

- **`bsl_sql`: columnar result format.** `rows` are now arrays of values positioned by `columns` instead of JSON objects `{column: value}` — column names are not duplicated in every row, saving context on wide result sets. The format is explicitly described in the tool description.
- **Softened mass-mode wording** in the descriptions of `get_function`/`get_class`/`get_object_structure` and the `names`/`full_names` parameters: batch ONLY when the whole set is definitely needed and one element's result cannot make the rest unnecessary; when filtering candidates — one at a time with early stopping; "when in doubt — one at a time". Encodes the ut-test benchmark conclusion from 0.28.0 (token front-loading and over-fetch caused by unconditional batching). Relevant for configurations with `[mcp].mass_mode_tools` enabled.

### Documentation

- `docs/operations.md` — indexer administration procedures (adding a repo to daemon.toml+serve.toml, daemon config hot-reload, restart/rebuild, "MCP not responding" diagnostics), moved out of session rules.

### Tests

- `parse_object_header_xml` (ru synonym priority, break at `<ChildObjects>`, object without a synonym), `apply_sections_filters_top_level_keys` (None/empty list/non-object — unchanged), columnar `collect_rows_*` tests. Full workspace green.

## [0.28.0] — 2026-06-10

**Bulk mode (`names[]`/`full_names[]`) is OFF by default; enabled via the `[mcp].mass_mode_tools` allowlist in `daemon.toml`.**

### Changed (default behavior)

- **Bulk mode is now opt-in and off by default for all tools.** A benchmark on ut-test (10 business tasks, Opus, ci arm with the new tools vs baseline) showed the promised token savings from batching **do not hold**: total input tokens went up (+37% on the run) while turns barely changed. Mechanism: a batch *front-loads* data (all targets land on the first turn and are re-read on every subsequent one), provokes over-fetch (the model pulls more targets than it would sequentially with an early stop), and on "hot" (non-unique) names `get_function`/`get_class` the response inflates without bound. Tokens are the dominant cost on a subscription, so bulk mode is disabled by default.
  - Controlled by a new `daemon.toml` section:
    ```toml
    [mcp]
    # a tool in the list advertises its plural param (the model decides whether to batch);
    # not in the list -> the server does not offer batching. Empty/absent -> off for all.
    mass_mode_tools = ["get_object_structure"]
    ```
  - A tool **in the list** advertises `names[]`/`full_names[]` in `tools/list` (model can batch). **Not in the list** — the server strips the plural param from the schema (`list_tools`) and rejects `tools/call` carrying it (`-32602 Invalid params`, double protection).
  - **Compatibility:** single `name`/`full_name` works as before. This is a behavior change vs 0.26.0/0.27.0, where bulk mode was on for all three tools. To restore the old behavior — `mass_mode_tools = ["get_object_structure", "get_function", "get_class"]`.

### Tests

- `strip_mass_mode_param` (removes the plural param from the schema + trims the description), `apply_mass_mode_tools` (empty list → off; allowlist → membership), parsing of `[mcp].mass_mode_tools` and the empty default. `cargo test`: code-index-core green.

## [0.27.0] — 2026-06-10

**Bulk mode now runs in parallel: `names[]`/`full_names[]` elements are processed concurrently instead of in a loop.**

### Changed

- **Mass-mode `get_function`/`get_class`/`get_object_structure` executes IN PARALLEL.** Previously — a sequential loop with `await` per element (and in `get_object_structure` — a `map` over a single connection). Now each element checks out its own read-only connection from the `StoragePool` and runs in `spawn_blocking` — synchronous rusqlite no longer blocks the shared async runtime, and parallelism is naturally bounded by the pool semaphore (`pool_size`, default 4). The response format is UNCHANGED: `{results:[...]}` strictly in request order, per-element `{error}` preserved (a broken element does not fail the batch), `_meta` stripped from elements as before. Single `name`/`full_name` path untouched. Internals: shared helper `mcp::tools::mass_map` (pool checkout + `spawn_blocking` + ordered assembly), sync cores `get_function_with`/`get_class_with` extracted from `get_function`/`get_class`, the `resolve_one` closure in bsl-extension became a free fn. Live serve smoke (`oleg`, local KA 1.1 repo): a batch of 4 heavy objects — 3.0 ms vs 19.7 ms as the sum of singles.

### Tests

- `mass_map_runs_concurrently_and_preserves_order` (4 elements × 100 ms on a 4-connection pool — wall < 250 ms, order intact), `mass_map_on_single_pool_stays_correct` (pool of 1 — degrades to sequential without losing correctness), `get_object_structure_batch_non_string_element` (non-string element → `{error}` in its slot), `get_object_structure_batch_empty_list`. `cargo test`: code-index-core 267 + bsl-extension 19, 0 failed.

## [0.26.0] — 2026-06-10

**Bulk mode for tools: structures/bodies of several objects in one call (`get_object_structure`, `get_function`, `get_class`).**

### Added

- **`names: [...]` parameters in `get_function` and `get_class`** — bodies of several functions/classes in one call instead of a series. Response is `{results: [...]}` in request order (each element is `{result: [...records...], hint?}` without the internal `_meta`); a missing name yields an empty `result` + `hint` in its slot and does not fail the batch. Single `name` unchanged (backward compatible). `find_symbol` intentionally untouched (stays single — it has its own `NameParams`). Candidates chosen by series statistics of a real run (`get_function` is 2nd by groupable calls after `get_object_structure`). Heavy `bsl_sql`/`get_object_profile` are NOT made bulk: high reuse would bury their expensive per-object cache in a blob (needs a dissolving layer — separate task).
- **`full_names: [...]` parameter in `get_object_structure`** — request the structure of several objects in a single call instead of a series of single ones. Response is `{results: [...]}` in request order; a missing object yields `error` + `did_you_mean` in its own slot and does not fail the rest of the batch. Single `full_name` works as before (backward compatible). Why: on tasks like "structures of these N documents/catalogs/registers" the model groups independent objects into one call — fewer round-trips, less history re-reading (the main token cost). Elements are processed in a sequential loop on one connection (`get_object_structure` is a cheap indexed SELECT, parallelism is unnecessary). Probe on ut-test (Opus, headless): the model adopts the bulk mode on its own from the tool description — in 3/3 tasks it sent `full_names` as a batch (4 documents / 5 catalogs / 3 registers) without any hint about the parameter format.

### Tests

- New integration test `get_object_structure_batch_full_names` (3 objects: 2 exist + 1 missing — order, structure, graceful error in slot). `bsl-extension` green (17 tests, 0 failed). Live MCP smoke (ut-test) confirmed the bulk mode.

## [0.25.0] — 2026-06-09

**Document posting properties in `get_object_structure`; BSL call-graph accuracy; token trimming on hot names; false `indexing` status removed.**

### Added

- **`posting` section in `get_object_structure`/`get_object_profile`** — document posting properties from the root `<Properties>`: `Posting`, `RealTimePosting`, `RegisterRecordsDeletion`, `RegisterRecordsWritingOnPost`. Documents only (other objects have no such section). Previously these properties were not indexed — an agent could not learn the posting/unposting movement behaviour and fell back to guessing (on 1C-Trade business questions: "what happens to register records on unposting?" → assumption instead of fact). Live smoke on ut-test: `Document.РеализацияТоваровУслуг` → `posting: {Posting: Allow, RealTimePosting: Deny, RegisterRecordsDeletion: AutoDeleteOff, RegisterRecordsWritingOnPost: WriteSelected}`.

### Fixed

- **BSL call graph now captures function calls inside expressions.** `get_callers`/`get_callees` silently returned a handful of edges instead of thousands: the parser walker caught only `call_statement` (a procedure call as a statement), while function calls inside expressions (`Result = Module.Func(...)` — `method_call` nodes inside assignment/condition/arguments) were lost entirely. Rewrote `visit_body_for_calls` + `visit_node` (helper `record_method_call`). On ut-test (1C-Trade 11.5): `get_callers(ЗначениеРеквизитаОбъекта)` 1 → 4560 edges; `proc_call_graph` direct 458011 → 790835.
- **False `indexing` status from the daemon's `path_status`.** Previously `std::fs::canonicalize()` was called on EVERY request — FS-dependent, and under the load of reindexing neighbouring repos it mismatched, reporting a false `indexing` on a ready repo. Now an FS-free match by normalized key (symmetric to `/health`): exact match or the nearest parent — the longest tracked-key prefix.

### Changed (context trimming on large repos)

- **Location cap (`LOCATION_CAP=50`)** in `find_symbol`/`get_function`/`get_class`: on a super-hot name (352 definitions of `ОбработкаПроведения`) the location payload drops 32K → 5.3K tokens (−84%); on truncation — `{truncated, total, shown}`. A unique name + `path_glob` still returns the body.
- **`get_call_tree` deduplication** (`expanded: HashSet`): a node with many parents is expanded once (repeat → `{name, repeated:true}`). callers depth=5: 178K → 8.4K tokens (−95%).
- **`grep_body` on 0 matches** now hints: for a compound name `Object.Field` use a short anchor (just `Object` or just `Field`) or a regex with flexible whitespace (`Object\s*\.\s*Field`) — the identifier may be split by formatting or live inside query text.
- Tool descriptions in `mcp/mod.rs` synced with behaviour: `search_*`/`find_symbol` return locations without bodies; `get_function`/`get_class` on multiple matches omit bodies and ask to narrow `path_glob`.

### Tests

- Entire workspace green (265 tests, 0 failed). New unit tests: parsing the `posting` section, absence of the section for non-documents. Full ut-test reindex (57102 files) + live MCP smoke confirmed `posting` and the call graph.

## [0.24.0] — 2026-06-08

**Per-repo pool of read-only connections in `serve`: requests to one repo are no longer serialized behind a single mutex.**

### Added

- **Per-repo connection pool (`storage::pool::StoragePool`).** Previously each repo in `serve` was served by a single `rusqlite::Connection` behind a `tokio::sync::Mutex` — any tool held the mutex for its whole duration, so a heavy query (`bsl_sql` up to 8 s, a full `grep_code`, recursive `find_path`/`get_call_tree`) delayed ALL other requests to the SAME repo, including an instant `get_function`. Now `serve` keeps several read-only connections to one `index.db` and reads the index in parallel (SQLite in WAL mode is designed for many readers). Connections are opened lazily up to `pool_size` and returned to the pool when the request finishes; the number of concurrently checked-out connections is bounded by a semaphore. Does not affect data/results — concurrency only.
- **`[pool]` section in `serve.toml`** — three optional parameters (defaults in parentheses): `pool_size` (4) — connections per repo; `per_conn_cache_kib` (16384 = 16 MB) — page-cache per connection; `busy_timeout_ms` (5000) — wait on brief locks during the daemon's checkpoint/backup. A missing section or fields fall back to defaults; `0` is sanitized (`pool_size`→1, `cache`→default). **The default is memory-neutral:** 4 × 16 MB = 64 MB per active repo — the same as the previous single connection (`cache_size=-64000`).
- **`busy_timeout` on read-only connections** (previously unset → default 0): a brief `SQLITE_BUSY` during the daemon's checkpoint/backup window is now waited out instead of becoming an error.

### Changed

- `RepoEntry.storage` field: `Option<Arc<Mutex<Storage>>>` → `Option<Arc<StoragePool>>`; method `local_storage()` → `storage_pool()`. Tool handlers (core `mcp::tools::*` and all extension BSL tools) acquire a connection via `pool.get().await` instead of `lock().await`. Internal change — no effect on the MCP contract (`tools/list`, response shapes).
- `CodeIndexServer::from_federated` takes a `PoolConfig` (from `serve.toml [pool]`); mono-mode and test constructors use the default `PoolConfig` / a single-connection pool (`StoragePool::single`).

### Tests

- Pool unit tests: connection reuse, `0` sanitization, single-mode; **"a heavy holder does not block a second request" at `pool_size>=2`** and the contrast **"a single-connection pool serializes"**. Whole workspace green (262+ tests).

## [0.23.0] — 2026-06-08

**Universal call graph: recursive `find_path` and `get_call_tree` over the `calls` table (any language). The BSL-specific `find_path` was renamed to `find_path_bsl`.**

### Added

- **MCP tool `find_path(repo, from, to, max_depth=5, language?)`** — shortest path in the call graph from function `from` to `to` (iterative cycle-safe BFS over unique nodes of the `calls` table, `max_depth` in [1..10]). Universal, any language — previously the recursive path walk lived only in the BSL extension (`proc_call_graph`); now the core (`code-index`) has it too. Returns `{from, to, found, path: [{caller, callee, line}], max_depth}`. On an empty result — a `hint`.
- **MCP tool `get_call_tree(repo, root, direction='callees', max_depth=3, max_nodes=200, language?)`** — call tree from function `root` up to `max_depth`. `direction`: `callees`/`down` (what root calls, downstream) or `callers`/`up` (who calls root). Previously the core exposed only a single level (`get_callers`/`get_callees`). Returns a flat edge list `[{caller, callee, line, depth}]` and a nested tree `{name, children}`. When `max_nodes` is reached — `truncated=true`.
- Federation routes `/federate/find_path` and `/federate/get_call_tree`; `CallEdge`/`CallTreeEdge` types in `storage::models`; storage methods `find_call_path` (iterative cycle-safe BFS over UNIQUE nodes — each node expanded once, no blow-up on cycles/duplicate edges) and `get_call_tree` (recursive CTE), seek via `idx_calls_caller`/`idx_calls_callee`. Unit tests for direct edge, two hops, depth limit, language filter, tree directions and `max_nodes` truncation.

### Compatibility

- **The BSL tool `find_path` was renamed to `find_path_bsl`** (module `find_path_bsl.rs`, struct `FindPathBslTool`). Its behavior and parameters (`from`, `to`, `max_depth`, over `proc_call_graph` with `call_type`) are unchanged — only the name. The name `find_path` is now taken by the universal core tool. Clients that called the BSL `find_path` directly must switch to `find_path_bsl`.
- On the `bsl-indexer` build in federated mode, `tools/list` returns two more tools (the universal `find_path` + `get_call_tree`); the BSL tool set is unchanged in count (a rename).

## [0.22.0] — 2026-06-08

**Cyrillic in `bsl_sql` and graph tools (case-insensitive search over Russian names) + fuzzy word-based FTS for functions/classes + lighter search payload + `sections` parameter for `get_object_profile`.**

### Fixed

- **SQLite `lower()`/`upper()` now handle Cyrillic.** The built-in SQLite functions fold case for Latin only, so in `bsl_sql` a query `WHERE lower(name) LIKE '%эдо%'` over Russian metadata names returned nothing and the agent fell back to enumeration. We register Unicode-aware `lower`/`upper` (Rust `String::to_lowercase`/`to_uppercase`) overriding the built-ins on every DB-open path (`register_sql_functions` next to `register_regexp`). Verified on production UT-11: `lower('ЭДО')='эдо'`, the slice `WHERE lower(name) LIKE '%эдо%'` over `metadata_objects` — 0 → 336 objects.

### Added

- **Case-insensitive reverse lookup over Cyrillic in `find_references`.** Columns `data_links.to_object_key` (= `lower(to_object)`) and `role_rights.object_name_key` (= `lower(object_name)`), computed in Rust (SQLite `lower()` does not fold Cyrillic), plus indexes `idx_dl_to_key` / `idx_rr_object_key`. `find_references` (`data_refs` / `role_rights`) finds references to an object regardless of the Russian name's case. Backfilled on (incremental) graph population.
- **`sections` parameter for `get_object_profile`** — `['structure'|'forms'|'modules'|'data_links']` narrows the response (cost lever: `['structure']` returns only attributes/tabular sections/dimensions/resources, without forms, modules and links).

### Changed

- **Fuzzy word-based FTS for `search_function`/`search_class`.** OR between query words, prefix terms, bm25 ranking (name outweighs `qualified_name`/docstring/body). Accepts a multi-word description ("расчёт цены продажи реализация"), no single exact identifier required; on 0 matches — a `hint` field. Query normalization — `sanitize_fts_query`.
- **Lighter `search_function`/`search_class` payload** — without function/class bodies: only name, `qualified_name`, path, line range, signature, truncated docstring (200 chars), `override_*`. Previously up to 20 results with full bodies bloated the response to tens of thousands of characters.
- **Compact `get_file_summary` map** for files with > 120 functions — names + lines + `override_type` without signatures/docstrings (guard against bloat on large modules).
- **`grep_text`/`grep_code`: `regex` is now optional** (can search via the `query` alias); grep-tool parameters forwarded through federation in lockstep.

### Compatibility

- **BSL index schema: added `*_key` columns** (`data_links.to_object_key`, `role_rights.object_name_key`) with `DEFAULT ''` + indexes — additive, existing queries unaffected. On older indexes the keys are backfilled on the new binary's first start.
- **Existing-DB migration (`migrate_schema` hook on `LanguageProcessor`).** Before `apply_schema_extensions`, the language processor idempotently adds missing `*_key` columns via `ALTER TABLE ADD COLUMN` (no-op on a fresh DB). Without this, upgrading on top of a 0.20.0/0.21.0 DB broke: `CREATE TABLE IF NOT EXISTS` does not add a column to an existing table, and the subsequent `CREATE INDEX` on the missing column aborted the whole DDL batch, so the `role_rights`/`metadata_code_usages` tables were not created and `find_references` did not work.
- The `lower()`/`upper()` override changes behavior only for Cyrillic (Latin — as before); internal queries and FTS are untouched.
- Workspace version 0.21.0 → **0.22.0** (minor — new functionality + a fix).

## [0.21.0] — 2026-06-07

**1C data-graph expansion and per-object "impact map" (reverse links + code usage + role rights) + text-file storage moved to a compressed format (`migrate_v5`).**

### Added

- **New configuration-level edge kinds in `data_links`.** The parser `bsl-extension/src/xml/metadata_refs.rs` (event-based `quick_xml`) adds, alongside the object edges (`attr`/`tabular_attr`/`register_dim`/`recorder`/`owner`), four links: `subsystem_content` (`from=Subsystem.X` — subsystem contents), `exchange_plan_content` (`from=ExchangePlan.X` — exchange-plan contents), `defined_type_content` (`from=DefinedType.X` — defined-type targets, reusing `object_attributes::classify_type`), `functional_option_location` (`from=FunctionalOption.X`, `from_path` = full `Location`). On production UT: subsystem_content 22762, exchange_plan_content 9302, defined_type_content 4728, functional_option_location 564.
- **Table `role_rights`** (`repo, role_name, object_name, right_name`, UNIQUE + indexes by object and by role) — role rights on objects from `Roles/<X>/Ext/Rights.xml` (only `<value>true</value>`). A right is an attribute of the role↔object pair, hence a separate table rather than a `data_links` edge. UT: 49,695 rights / 1236 roles / 6329 objects.
- **Table `metadata_code_usages`** — a reverse index of metadata-object usage IN `.bsl` CODE (module `bsl-extension/src/code_usages.rs`, a hand-written scanner with no `regex`): `manager` (`Документы.X` in code), `ref_type` (`"ДокументСсылка.X"` / `"DocumentRef.X"` in a string literal), `query` (a metadata path inside query text; the 3rd segment → `member_path`). UT: ~280k usages (query 149,835 / manager 110,194 / ref_type 20,420). `object_ref_key` is stored lowercased (SQLite `lower()` does not lowercase Cyrillic) — for a pinpoint lookup filter by the exact `object_ref`.
- **MCP tool `find_references`** — a per-object "impact map" in one call: reverse `data_links` (by `to_object`) + `metadata_code_usages` + `role_rights`, broken down by kind with samples (`limit`).
- **MCP tool `bsl_sql`** — an arbitrary read-only `SELECT`/`WITH` over a repo's `index.db` (the long tail of metadata/graph queries with no dedicated named tool). Guard: `SELECT`/`WITH` only + `Statement::readonly()` + row cap + timeout.
- **MCP tool `get_object_profile`** — a full object "passport" in one call (structure + forms + modules + data links) instead of a series of `get_object_structure`/`get_form_handlers`/`get_data_links`.
- BSL tool count 8 → **11**. All new tables are maintained incrementally in the daemon's watcher loop (rebuild by path component / per-`.bsl`).

### Fixed

- **`attributes_json` merge with extensions present** (`object_attributes::ObjectStructure::merge_from`) — attributes from the base configuration and extensions are merged rather than clobbering each other. `extension_override` edges are accounted for when (re)building the call graph.

### Compatibility

- **Index schema v4 → v5.** Text-file storage (md/xml/yaml/json/toml/sh…) moved from `text_files(content TEXT)` + external-content FTS to `text_contents(content_blob BLOB)` (zstd) + a contentless `fts_text_files` fed from Rust. `migrate_v5` migrates existing indexes IN PLACE on the first start of the new binary; fresh DBs are created directly in the new schema. `search_text`/`grep_text`/`read_file(text)`/`stat_file` behave as before. (Implementation — a separate `feat(core)` commit.)
- **External consumers that read the index's text directly via the `text_files` table must switch to `text_contents`** (raw → zstd-decode). In particular `code-index-guard` (`serveability`) was updated in lockstep: otherwise on a migrated DB the query fails with `no such table: text_files` and native `Read` blocking silently turns off.
- Additive for BSL: the new tables / edge kinds / tools do not break existing responses.
- Workspace version 0.20.0 → **0.21.0** (minor — new functionality).

## [0.20.0] — 2026-06-06

**`_meta.file_mtimes` in search-tool responses + an early daemon signal `POST /mark-dirty` — for write-triggered lazy cache revalidation in `mcp-cache-ci` (#1471).**

### Added

- **`_meta.file_mtimes` in MCP tool responses.** Alongside the existing `_meta.dependent_files`, serve now returns a `{<rel_path>: <index_mtime>}` map (unix seconds from the `files.mtime` column) for each dependent file. This is the input for write-triggered lazy revalidation in `mcp-cache-ci`: the proxy compares the index mtime against the observed mtime from `mark-dirty` and caches a response only once the index has caught up with disk (`index_mtime >= observed`). Implemented in `wrap_with_meta` (batched via the new `Storage::mtime_for_path`), applied to all cacheable search tools (`search_function`/`get_function`/`grep_body`/`grep_code`/`read_file`/`get_file_summary`/...). `stat_file` is unaffected (non-cacheable, carries no `_meta`).
- **The daemon sends `POST /mark-dirty` on FS events.** At the start of batch processing (BEFORE reparse/commit), in addition to `POST /invalidate` after commit, the daemon sends cache-ci `{repo, files:[{path, mtime}]}` with the observed mtimes of changed files (`PathEntry::effective_alias()` as `repo`). The proxy marks dependent entries dirty immediately, shrinking the window in which the cache could serve stale data; the flag is cleared by the mtime check on the cache-ci side. Best-effort: errors and 404s (a cache-ci without support) are logged and never block the daemon. New `CacheClient::mark_dirty_files`, helper `collect_dirty_paths` in `worker.rs`.

### Compatibility

- **Additive, not breaking.** `_meta.file_mtimes` is a new field next to `dependent_files`; old consumers ignore it. `mark-dirty` is a separate channel in addition to `invalidate`. Full effect requires `mcp-cache-ci` ≥ 0.4.0; with an older cache-ci `mark-dirty` returns 404 (swallowed) and `file_mtimes` is ignored.
- BSL tools (`bsl-extension`) do **not** yet emit `file_mtimes` (follow-up): for dirty files depended on only by BSL responses, cache-ci keeps forwarding while the path is dirty (safe degradation).
- Workspace version 0.19.2 → **0.20.0** (minor — new functionality).

## [0.19.2] — 2026-06-06

**Renaming a file to a new name no longer leaves an orphaned index row under the old name.**

### Fixed

- **The watcher now correctly removes the old name on a file rename.** Previously the `notify` event `Modify(Name(RenameMode::From))` — delivered for the old name's path that no longer exists — was either dropped by the `!path.is_file()` check or turned into `Modified` and silently swallowed by `NotFound` during hashing, leaving the old-name row as a phantom in the index until the next full reindex (showing up in `stat_file`/`list_files`/the graph with stale data). The classification logic was extracted into `classify_event`: directories are ignored, and `Create`/`Modify` on a path missing from disk are treated as `Deleted`. Covered by the test `test_classify_event_rename_from_becomes_delete`. (Atomic-save `tmp`→rename over an existing file worked before and still works — the target path stays a file.)

## [0.19.1] — 2026-06-06

**The daemon's incremental path now writes `mtime`/`file_size` for new and changed files — previously the watcher left these fields NULL.**

### Fixed

- **`Storage::upsert_file` now persists `mtime`/`file_size`** (added to `INSERT` and `ON CONFLICT DO UPDATE` with `COALESCE` so real values are never clobbered by `NULL`). With a live daemon, creating/changing a file used to leave the `files` row with `mtime = NULL` and `file_size = NULL`: the values did reach the `FileRecord` (both from full indexing and from the watcher via `std::fs::metadata`), but `upsert_file` dropped them. A real `mtime` was written only by the separate `update_file_metadata`, which on the write path runs only for unchanged-hash files — so freshly created/just-changed files (the "hottest" ones) kept an empty `mtime`. This hurt `stat_file`/`list_files`, the cheap freshness check (`code-index-guard`), and the phase-1b "fast skip by mtime+size" on subsequent full reindexes. Both paths now write `mtime`/`file_size` in one place. Covered by the regression test `test_upsert_file_persists_mtime_and_size`.

## [0.19.0] — 2026-06-05

**Online extras-layer updates during live editing: after a file edit the call graph, data links and object structure refresh incrementally right in the daemon's watcher loop — surgically (cost scales with the edited file, not the graph), no restart, no full XML walk.**

Previously the daemon built the extras layer (`proc_call_graph`, `data_links`, `metadata_objects.attributes_json`, `metadata_forms`, `event_subscriptions`) once at worker startup and it went stale until restart: `find_path`/`get_callers`/`get_data_links`/`get_object_structure` returned an outdated view during an editing session. The full rebuild (`run_index_extras`) is expensive (~2s, walks all XML), so it was not run on every save.

### Added

- **Incremental extras update in the daemon's watcher loop** (after `commit_batch`), routed by changed-file type:
  - `.bsl` → **per-file** update of the `direct` call-graph layer: only the edited file's edges are touched (previous ones from the `direct_edge_files` side-map, current ones from the core `calls` table), cost independent of graph size. On KA 1.1 (~123k edges) — ~0 ms vs ~2 s for a full layer rebuild;
  - object XML (Catalogs/Documents/Registers/…) → per-object update of `data_links` (by `from_object`) and object structure (`attributes_json`);
  - `Form.xml` / `EventSubscriptions/*.xml` → per-file row update + slice-rebuild of the matching graph layer (`form_event` / `subscription`);
  - `Configuration.xml` (object-set change) → full `run_index_extras`.
- New `LanguageProcessor::index_extras_for_files` method (default no-op; universal languages unaffected). BSL implements it via `run_incremental_extras`.

A helper table `direct_edge_files` was added for the per-file graph update (maps direct edges to their source files); `proc_call_graph` stays deduplicated, so `find_path`/`find_data_path` are unaffected. Worker logs now include extras-update timing (full and incremental paths). Equivalence of incremental updates to a full rebuild is covered by tests (`incremental_object_xml_matches_full`, `incremental_call_graph_direct_matches_full`, `incremental_direct_shared_edge_survives`).

## [0.18.0] — 2026-06-05

**Targeted BSL tooling and CLI refinements from the E2E comparison with `rlm-tools-bsl` (KA 1.1): subscription filter by short module name, a `search_terms` hint when enrichment is empty, a fast `index --force` plus a PID-lock for one-off indexing, and an updated `get_object_structure` description.**

### Added

- **`search_terms` returns a `hint` on empty results with an empty enrichment table** — states that `bsl-indexer enrich` has not been run for the repo and points to `grep_body`/`grep_code`/`search_function`/`get_function`. Previously an empty answer read as "no matches" and wasted the call. (E1)
- **PID-lock for the `index` command** — two concurrent `index` runs on the same path no longer fight over SQLite (RAII lock on `index.lock` next to `index.db`). Shares the daemon mechanism (`acquire_at`). (A2)

### Changed

- **`get_event_subscriptions`: the `handler_module` filter matches both the full name (`CommonModule.X`) and the short one (`X`)** via a suffix `LIKE '%.X'`. Previously a short name found no subscription. (D1)
- **`index --force` recreates `index.db` from scratch** instead of upserting over the existing DB. On a large DB the old path was pathologically slow (full load into RAM + per-file upsert); deleting the file turns `--force` into a fast fresh path with the same result. (A1)
- **Updated `get_object_structure` description** — reflects the full structure (attributes with types, tabular sections, register dimensions/resources, `enum_values`, `predefined`, always-present base sections) and explicitly notes that object XML is not indexed as text (don't search it via `list_files`/`grep_text`). (D2)

## [0.17.0] — 2026-06-05

**`get_object_structure`: a `predefined` section — names of predefined items (Catalog / ChartOfAccounts / ChartOf*).**

Closes the C2 gap from the E2E comparison with `rlm-tools-bsl`: predefined items (`Catalogs.Quality.New`, etc.) live in a separate `<Object>/Ext/Predefined.xml` and previously required manual XML reading. Now there is a `predefined` section right in the object structure.

### Added

- **`get_object_structure` returns a `predefined` section** — names of an object's predefined items from the adjacent `<Object>/Ext/Predefined.xml` (`<Item>/<Name>`). Populated during indexing for `Catalog`/`ChartOfAccounts`/`ChartOfCharacteristicTypes`/`ChartOfCalculationTypes`; absent for objects without predefined items. Verified on live KA 1.1: `Catalog.Качество` → `["Новый"]`, `Catalog.СтатьиЗатрат` → `["СписаниеНДСНаРасходы","СписаниеНДСНаРасходыПрочие"]`.

## [0.16.0] — 2026-06-05

**1C metadata tools: `get_object_structure` now returns the full structure (including enum values), a new `get_register_writers` tool (register recorders / document movements), subscription event names normalized to Russian.**

A round of BSL-layer improvements following the E2E comparison with `rlm-tools-bsl` on the KA 1.1 configuration: the main gap "which documents write movements into a register" is closed, and `get_object_structure` is no longer a stub for enums and always returns a predictable response shape.

### Added

- **New MCP tool `get_register_writers`** — register recorders and document movements from the declarative `<RegisterRecords>` set (recorder edges of the `data_links` graph). For a register (`AccumulationRegister.ТоварыНаСкладах`) it returns the documents writing movements in `writers`; for a document (`Document.РеализацияТоваровУслуг`) the target registers in `writes_to`. A single call covers both directions — no need to know the object kind in advance. 8 BSL tools on top of the 18 universal ones.
- **recorder edges in the `data_links` graph** — the "document → register" relation (`link_kind=recorder`) from a document's declarative movement set. `get_data_links(register, direction=in)` now lists recorder documents (previously empty — register movements were not modeled by the graph). The source is the document XML `<RegisterRecords>`, not posting-code parsing — no false positives.
- **`get_object_structure` for enumerations** — an `enum_values` section with the enum's values (previously `Enum.*` returned an empty structure). The `Enums` folder was added to the metadata indexer's walk.

### Changed

- **`get_object_structure` returns the full object structure** — attributes with types in 1C notation (`СправочникСсылка.X`, composite via `|`), tabular sections with columns, register dimensions/resources. Previously a documented stub `attributes: null`.
- **`get_object_structure` always emits the base sections** `attributes`/`dimensions`/`resources`/`tabular_sections` (empty as `[]`, not omitted). The consumer distinguishes "the section is absent" from "the tool did not return it" and does not fall back to raw XML.
- **Event names in `get_event_subscriptions` normalized to Russian** (`OnWrite`→`ПриЗаписи`, `Posting`→`ОбработкаПроведения`, etc.); the filter is bidirectional — accepts both the Russian name and the English platform enum.

### Fixed

- **Updated the `read_file` tool docstring** — it returns content for code files too (zstd-decode from `file_contents`, Phase 2 v0.8.0+; the `category` field is `"text"`/`"code"`), not "text files only, empty for code". The old description was stale after Phase 2 and misleading.

## [0.15.0] — 2026-06-04

**`grep_text` and `grep_body`: output grouped by file + a `truncated` flag — path duplication eliminated, the same treatment `grep_code` got in 0.14.0.**

`grep_text` and `grep_body` returned a flat array of matches where the full file path was repeated in every match. With many matches in one file this bloated the response (and billed tokens when running over an API). `grep_code` switched to `{files: {"<path>": [...]}}` grouping back in 0.14.0; the same treatment is now applied to the two remaining grep tools.

### Changed

- **`grep_text` groups matches by file.** Response shape `[{path, line, content, context}]` → `{files: {"<path>": [{line, content, context?}]}, shown, limit, truncated}`. The path appears once as the `files` key; `context` is omitted when `context_lines=0`.
- **`grep_body` groups matches by file.** Shape `[{file_path, name, kind, …}]` → `{files: {"<path>": [{name, kind, line_start, line_end, match_lines, match_count?, context?}]}, shown, limit, truncated}`. `match_count` is omitted when there are ≤3 matches; `context` when `context_lines=0`.
- **Both tools now return `truncated`.** Storage methods `grep_text_filtered` and `grep_body_with_options` now return `(Vec, bool)` — the flag is set when `limit` or the 1 MB response byte cap is reached, just like `grep_code`. For the legacy `grep_body` path (no `path_glob`/`context_lines`) `truncated` is derived from hitting `limit`. The model sees that not everything is shown and can re-request with a larger `limit`.

### Compatibility

- Consumers that parsed the flat `grep_text`/`grep_body` array must read `result.files` (a "path → array of matches" object) instead of an array; the path field moved out of each match into the key. A one-off output-format breaking change — same as `grep_code` in 0.14.0.

## [0.14.2] — 2026-05-31

**`find_data_path`: traversal rewritten as BFS with a visited-set — combinatorial blow-up on dense link graphs eliminated.**

After 0.14.1 (ANALYZE fixed the seek), `find_data_path` traversal on a dense cyclic data-link graph could still expand millions of paths: the recursive CTE enumerated ALL paths up to max_depth without node deduplication (on KA 1.1 a dense node at depth=4 produced ~4.9M intermediate rows plus `path_json` memory growth).

### Fixed

- **`find_data_path` now uses BFS with a visited-set instead of the recursive CTE.** Each object is expanded exactly once (visited HashSet) → traversal is bounded by the reachable subgraph (thousands of nodes), not the number of paths (millions); link-graph cycles are no longer walked in circles. The same shortest-by-edge-count path from → to is returned. Terminal generic `*`-nodes have no outgoing edges and are not expanded. Each step is an index seek on `(repo, from_object)` (provided by the 0.14.1 ANALYZE). `find_path` (call graph) is untouched — its CTE stays, already made fast by ANALYZE.

## [0.14.1] — 2026-05-31

**`find_path`/`find_data_path`: graph-traversal timeouts on large BSL repos fixed (`ANALYZE` after graph build).**

On large configurations (KA 1.1 — `proc_call_graph` ~125k edges, `data_links` ~11.5k) `find_path`/`find_data_path` traversal hit timeouts: depth=3 on the call graph took ~240s. The cause — the repo's SQLite index had no statistics (`sqlite_stat1`), so in the recursive CTE step the planner used only the constant index prefix (`repo=`), scanning all repo edges on every iteration instead of seeking on `(repo, caller_proc_key)` / `(repo, from_object)`. Forcing the index via `INDEXED BY` did not help — statistics are the only lever.

### Fixed

- **The indexer now runs `ANALYZE` at the end of `run_index_extras`** — after rebuilding `proc_call_graph`/`data_links` (and the other BSL tables), SQLite statistics are gathered, and the planner uses a two-column seek in the recursive step of `find_path`/`find_data_path`. On KA 1.1: `find_path` depth=3 went 240s → 0.05s. `ANALYZE` costs ~0.6s on a 2.4 GB DB (it scans index B-trees, not the zstd content blobs) and runs on every (re)index in sync with the graph rebuild.

### Known limitations

- `find_data_path` on a **dense node at depth 4** still traverses millions of paths even after `ANALYZE` — the recursive CTE has no visited-set/cycle-detection and the 1C data-link graph is dense and cyclic. For typical queries and depth ≤3 it is instant; eliminating the blow-up on deep dense traversals is planned separately.

## [0.14.0] — 2026-05-30

**`grep_code`: matches grouped by file — the path is no longer repeated on every line.**

Previously `grep_code` returned a flat array where the full file path was duplicated in every match — yet matches often cluster in one file (dozens of hits with the same `path`). Matches are now grouped: the path is a key in the `files` object, with a list of `{line, content, context}` under it. On clustered results this noticeably shrinks the response.

### Changed

- **`grep_code` result format**: `{matches: [{path, line, content, context}], …}` → `{files: {"<path>": [{line, content, context}], …}, shown, limit, truncated}`. The path is stored once per file. The `context` field is omitted when `context_lines=0`. The `shown`/`limit`/`truncated` fields are unchanged.

### Compatibility

- **`grep_code` response format change** (`matches` array → `files` object grouped by path). Consumers read `result.files["<path>"]` instead of `result.matches[].path`. `grep_text`/`grep_body` are unaffected — their format is unchanged.

## [0.13.0] — 2026-05-30

**Compact JSON in MCP tool output instead of pretty.**

MCP tool output is consumed by the model, not a human — pretty-JSON indentation and newlines waste tokens for nothing. We switch response serialization to compact (`to_string` instead of `to_string_pretty`). ~30% saved on every tool response, especially noticeable for federation (remote repos) and text tools. The data itself is unchanged — same JSON, just unformatted.

### Changed

- **MCP tool response serialization is now compact** (`to_string`): `wrap_with_meta` (18 universal tools — read_file, grep_*, get_function, list_files, etc.), `to_json` (`get_stats`/`stat_file`/`health`), `format_unavailable`, federation forwarding (`federation_error` + per-repo `get_stats` aggregation).
- BSL-tools already emitted compact via `CallToolResult::structured` — unaffected.

### Compatibility

- The data format is unchanged — only pretty-formatting (indentation/newlines) was removed. Any JSON parser reads the result as before.
- **CLI output** (`--json`) and the `daemon.json`/`config.json` files stay pretty — they are human-readable and not on the model's hot path.

## [0.12.0] — 2026-05-30

**`grep_code`: default `limit` lowered 500→100 and an explicit `truncated` flag added.**

Based on real usage stats (a 2-month sample, ~240 `grep_code` calls): when the model sets `limit` itself, it picks ~20–40 matches (median 30), and specified 500 only twice out of a hundred calls. The old default of 500 (with a `path_glob`/`language` filter) inflated the response twofold versus native Grep (`head_limit` 250) — especially with `context_lines`. We lower the default to 100 and make truncation visible so the model can re-request a larger `limit` instead of treating a truncated list as complete.

### Changed

- **`grep_code` default `limit` 500 → 100** (new `GREP_CODE_DEFAULT_LIMIT` constant). Previously the default depended on the filter: 100 on full-scan / 500 with a `path_glob`/`language` filter; now a single default of 100. An explicitly passed `limit` works as before.
- **`grep_code` result format**: instead of a bare array `[{path, line, content, context}]`, it now returns an object `{matches: [...], shown, limit, truncated}`. `truncated=true` means the result was cut off by `limit` or the byte cap (1 MB) — there are more matches, re-request with a larger `limit`. Previously truncation was silent and read as "these are all matches".
- **`Storage::grep_code_filtered`** now returns `(Vec<GrepTextMatch>, bool)` — the second tuple element is the truncation flag.

### Compatibility

- **`grep_code` response format change** (array → object `{matches, …}`). Consumers that parsed the response as an array must read `result.matches`. `mcp-cache-ci` (uses only `_meta.dependent_files`) and federation forwarding are unaffected. `grep_text`/`grep_body` formats are **unchanged** — still arrays.

## [0.11.0] — 2026-05-30

**Optional whitelist of MCP tools via `[tools].enabled` in `daemon.toml`.**

The fight for your tokens and speed continues: the server can now be configured to expose only a subset of tools in `tools/list` instead of all 25 (18 universal + 7 BSL). Fewer schema tokens on every `initialize`, less confusion for weaker models when picking a tool, same functionality for stronger ones. Default behavior is unchanged — if there is no `[tools]` section or `enabled` is empty, all registered tools remain available (backward compatible).

### Added

- **`[tools]` section in `daemon.toml`** with an `enabled: Vec<String>` field. Empty array or missing section — all tools available. Filled — only listed names appear in `tools/list`; others are blocked at `tools/call` with `-32602 Invalid params: tool 'X' is disabled by [tools].enabled whitelist in daemon.toml`. Double protection is needed because the model may invoke a tool from its memory / system prompt bypassing `tools/list` — a `list_tools`-only filter would not stop that.
- **`CodeIndexServer::with_allowed_tools(Option<BTreeSet<String>>)`** — builder for setting the whitelist programmatically (used by `cli.rs`).
- **`CodeIndexServer::validate_whitelist(&BTreeSet<String>) -> Vec<String>`** — returns names that do not match any registered tool (typos, removed tools). Used by `cli.rs` for a startup warning.
- **Startup logs**: empty `enabled` → `[tools].enabled is empty — whitelist disabled, all tools available`; non-empty → `[tools].enabled whitelist active: N known tools enabled (M in list)` + warning on unknown names.
- **3 parsing tests** for the `[tools]` section in `daemon_core::config::tests` (`tools_section_default_empty`, `parses_tools_whitelist`, `parses_empty_tools_section`).

### Compatibility

- Fully backward compatible. Old `daemon.toml` without a `[tools]` section continues to work as before (all tools available). Default behavior matches v0.10.x.
- Minimum functionally safe set: `read_file`, `grep_code`, `get_function`, `find_symbol`, `list_files`, `get_stats`, `health`. Trimming below this (e.g., keeping only `grep_body` without `grep_code`) leads to blindness on imports / directives / module-level code and fallbacks via the expensive full `read_file` — the token savings will be destroyed.

## [0.10.4] — 2026-05-22

**Fix for publishing to the MCP registry: namespace case.**

The registry rejected `server.json` with a 403 — the namespace was given in lowercase (`io.github.regsorm`), while OIDC grants rights to a namespace that exactly matches the GitHub login (`io.github.Regsorm`). The case in `name`/`mcpName` is fixed. npm publishing already succeeded in 0.10.2/0.10.3; this patch completes the registration of the listing in the official registry.

### Fixed

- **`server.json` `name` and `package.json` `mcpName`** — namespace case `io.github.Regsorm/code-index` (exactly as the GitHub login).

### Changed

- **Workspace version** 0.10.3 → 0.10.4.

## [0.10.3] — 2026-05-22

**Fix for publishing to the MCP registry: description length.**

The registry rejected `server.json` with a 422 — the `description` field exceeded the 100-character limit. Shortened to 98. npm publishing already succeeded in 0.10.2; this patch completes the registration of the listing in the official registry.

### Fixed

- **`server.json` `description`** shortened to ≤100 characters (registry requirement).

### Changed

- **Workspace version** 0.10.2 → 0.10.3.

## [0.10.2] — 2026-05-22

**Auto-publish fix: a working workflow trigger.**

In 0.10.1 publishing did not fire — `publish-registry.yml` was on a `workflow_run` trigger, which GitHub only runs when the file is present on the default branch (`main`); releases, however, are tagged from a working branch. In addition, the `mcp-publisher` download pattern was picking up an extra asset.

### Fixed

- **`publish-registry.yml` trigger** switched from `workflow_run` to `push: tags: ['v*']` — works from any branch. Added a step that waits for the GitHub Release (the code-index archives) to be ready before `npm publish`, to eliminate a race.
- **`mcp-publisher` download** — exact asset pattern `mcp-publisher_linux_amd64.tar.gz` (previously `*linux_amd64.tar.gz` also matched `registry_linux_amd64.tar.gz`).
- The `mcp-publisher login github-oidc` and `publish` commands were verified against the actual CLI (v1.7.9).

### Changed

- **Workspace version** 0.10.1 → 0.10.2.

## [0.10.1] — 2026-05-22

**Publishing to npm and the official MCP registry.**

The public `code-index` can now be installed via `npx`/`npm` and is registered in the [official MCP registry](https://registry.modelcontextprotocol.io/) (`io.github.regsorm/code-index`). The Rust binary is still distributed via GitHub Releases — the npm package is only a thin wrapper that downloads the archive for the current platform on install. `bsl-indexer` stays private and is not published to the registry.

### Added

- **npm wrapper `@regsorm/code-index-mcp`** (the `npm/` directory): `package.json` with `mcpName`, `bin/cli.js` (transparently proxies arguments and stdio to the native binary), `scripts/postinstall.js` (downloads the `code-index-<platform>` archive from GitHub Releases and unpacks it with the system `tar`/bsdtar). Supports Windows x64, Linux x64, macOS arm64.
- **`server.json`** — the listing for the official MCP registry (npm package, stdio transport, the `serve` subcommand).
- **`.github/workflows/publish-registry.yml`** — after a successful `Release` on a `v*` tag: `npm publish` + `mcp-publisher publish`. The version is substituted from the tag. Requires the `NPM_TOKEN` secret.

### Changed

- **Workspace version** 0.10.0 → 0.10.1.

### Compatibility

- Fully backward compatible. There are no changes in the indexer code — only the distribution infrastructure.

## [0.10.0] — 2026-05-21

**1C data-link graph (data-graph): new BSL tools `get_data_links` and `find_data_path`.**

Complements the CALL graph (`proc_call_graph`) with a DATA-LINK graph — "object → object" edges built from the reference types of attributes, register dimensions, and tabular-section attributes. It closes a common "wandering through the structure" pattern: instead of a series of `get_object_structure`/`get_metadata_structure` calls to trace links by hand — a single graph traversal. (On the real "collapse stock by customs declaration" case this used to be 37 structure queries → now a single `get_data_links`.)

### Added

- **`data_links` table** in the `bsl-extension` schema (`SCHEMA_EXTENSIONS`, additive via `CREATE TABLE IF NOT EXISTS` — no migration required): `from_object`, `from_path` (attribute / `Table.attribute` / dimension), `to_object`, `link_kind` (`attr`/`tabular_attr`/`register_dim`), `is_composite`, `is_universal`. Indexes `idx_dl_from` (forward traversal) and `idx_dl_to` (reverse — "who references X").
- **`crates/bsl-extension/src/xml/object_attributes.rs`** — a parser for reference types from individual objects' XML (`Catalogs/<X>.xml`, `Documents/<Y>.xml`, registers). Type classification: a concrete `cfg:CatalogRef.Контрагенты` → an edge to `Catalog.Контрагенты`; a composite one (several `<v8:Type>`) → several edges (`is_composite`); a generic one (`cfg:CatalogRef` without a name, `cfg:AnyRef`, `cfg:DefinedType.X`) → a terminal `*`-node (`is_universal`, not expanded during traversal — protection against fan-out and noise); primitives (`xs:`/`v8:`) are discarded. A safety cap for pathological type lists (>30 concrete types → `*Multiple`).
- **`index_data_links`** in `index_extras::run_index_extras` — traverses the object XML and populates `data_links` via a full rebuild (like the rest of `index_extras`). On a large configuration (~1900 object XMLs / ~68 MB) — ~1.3–1.9 s; incrementality is not needed.
- **MCP tool `get_data_links(repo, object, direction=out|in|both, depth=1..4)`** — the neighborhood of an object in the data-link graph via a recursive CTE. `out` — what it references; `in` — who references it; terminal `*`-nodes are not expanded during traversal.
- **MCP tool `find_data_path(repo, from, to, max_depth=4)`** — a path (a chain of reference links) between two objects (BFS over `data_links`, analogous to `find_path` for the call graph).
- Both tools are registered in `BslLanguageProcessor::additional_tools` (now **7 BSL tools**, **25** in total in the `bsl-indexer` build), available through federation as well (`POST /federate/extension`). Parser unit tests (3 type cases, tabular section, dimensions, cap) and population tests.

### Changed

- **Workspace version** 0.9.1 → 0.10.0.

### Compatibility

- Fully backward compatible. The new table is created idempotently at startup; existing indexes and tools are untouched. The public `code-index` binary does not change — the feature lives only in `bsl-indexer` (`bsl-extension`).

## [0.9.1] — 2026-05-12

**Stage 3 of the migration to event-based cache invalidation: notifying `mcp-cache-ci` after reindexing.**

It closes the loop: file saved → daemon (watcher) detected it → reindexed into SQLite → **sent `POST /invalidate {file_paths: [...]}` to cache-ci**. Using `reverse_index` (populated in stage 2 via `_meta.dependent_files`), cache-ci surgically drops only the dependent entries; the rest of the cache hits are preserved.

### Added

- **`crates/code-index-core/src/daemon_core/cache_client.rs`** — `CacheClient` with a pool of `reqwest::Client` (timeout 2s, keep-alive 60s) and a list of target URLs. The `invalidate_files(&[String])` method POSTs to all targets in parallel; on failure (network, 5xx, timeout) — an `eprintln!` warning and we move on; it must not panic, and the TTL on the cache-ci side serves as a safety net.
- **`[[cache_targets]]` section in `daemon.toml`** + the `CacheTargetEntry { url: String }` struct in `daemon_core/config.rs`. Example:

  ```toml
  [[cache_targets]]
  url = "http://127.0.0.1:8011"
  ```

  Multiple entries are allowed (multi-cache-ci topologies: local Windows + remote rag-VM). Absence of the section (or an empty list) → the event channel is off, behavior as before v0.9.1.
- **Helper `worker::collect_invalidate_paths(root, batch)`** — collects a deduplicated list of relative file paths from a batch of FS events. It accounts for all types (Modified/Created/Deleted) — deleting a file must also drop the associated cache entries.
- **`cache_client: Option<Arc<CacheClient>>` parameter** in `worker::run_worker` and `runner::spawn_worker`. It is threaded through from `runner::run` and `runner::handle_reload` (reload recreates `CacheClient` from the new config for added folders; existing workers keep their client until a daemon restart).
- **Unit tests** for `cache_client.rs`: empty targets → `is_empty()`; trailing slashes are stripped; an invalid target does not panic (connection refused → 0 successes). Tests for config.rs `cache_targets_default_empty` and `parses_cache_targets_list`.

### Changed

- **`worker::run_worker` signature** — a new trailing parameter `cache_client`.
- **`runner::spawn_worker` signature** — the same.
- **`commit_batch()` now returns a check result** — if the commit failed, no invalidate is sent (there is no new data in the index anyway; let cache-ci keep serving the old data — it will be corrected either on the next successful batch or via TTL).
- **Workspace version** 0.9.0 → 0.9.1.

### Compatibility

- `daemon.toml` without `[[cache_targets]]` — fully functional (behavior as before v0.9.1, no network traffic to cache-ci).
- `daemon.toml` with `[[cache_targets]]` — the event channel is activated automatically at startup.
- The `run_worker` / `spawn_worker` API — the signature changed (additive last param). External clients of the `code-index-core` crate (if any) must pass `None` for compatibility.

### Architecture (final state of the chain)

After v0.9.1 + cache-ci 0.2.0:

1. **The daemon's read-tools** return `{result, _meta: {dependent_files: [...]}}` (v0.9.0).
2. **`mcp-cache-ci`** on cache-fill writes `cache_key → file_paths` into `reverse_index` (cache-ci 0.2.0).
3. **The daemon watcher** on an FS event → reindex → `commit_batch` → `cache_client.invalidate_files(...)` → cache-ci drops surgically via `reverse_index` (v0.9.1).
4. **TTL fallback** — the third echelon of the safety net: if an event is lost (network, daemon crash, ReadDirectoryChangesW buffer overflow), the entry expires on its own after 600s/3600s.

## [0.9.0] — 2026-05-12

**Phase 2 (a stage of the migration to event-based cache invalidation): `_meta.dependent_files` in read responses.**

All MCP data tools now return a unified JSON format:

```json
{
  "result": <prev plain payload>,
  "_meta": { "dependent_files": ["src/X.bsl", "src/Y.bsl"] }
}
```

`dependent_files` is the list of file paths the response was built from. The intended consumer is `mcp-cache-ci`: on cache-fill it registers `cache_key → file_path` links in `reverse_index` and then surgically drops the affected entries on a signal from the daemon after a file is reindexed (stage 3, in preparation).

### Compatibility (BREAKING CHANGE to the response format)

All read-tool clients must be ready for the new `{result, _meta}` structure:

- Before: `search_function` returned a flat array `[FunctionRecord, ...]`.
- Now: `{"result": [FunctionRecord, ...], "_meta": {"dependent_files": [...]}}`.

For the existing consumer (`mcp-cache-ci` 0.2.0+) the behavior is backward compatible: cache-ci parses `_meta.dependent_files` if present, otherwise works as before (insert without dependencies, TTL fallback).

Tools **without** the wrapper (response format unchanged):

- `health` — non-cacheable.
- `get_stats` — diagnostic; its format is extended across federation, and a wrapper would break the aggregation.
- `stat_file` — trivial single-file.

### Added

- **Wrapper helpers in `crates/code-index-core/src/mcp/tools.rs`:**
  - `wrap_with_meta<T: Serialize>(result, dependent_files)` — final serialization into `{result, _meta}` with deduplication of file paths.
  - `collect_paths_via<R>(storage, records, extract: fn(&R) -> file_id)` — collect paths from a vec of records via an extractor.
- **Wrapper helpers in `crates/bsl-extension/src/tools/mod.rs`:**
  - `wrap_with_meta(result: Value, dependent_files: Vec<String>) -> Value` for BSL extension tools.
  - `wrap_error(error_value: Value) -> Value` — even on error the format is unified.
- **Support for `_meta.dependent_files` in core data tools:**
  - `search_function`, `search_class` — DISTINCT file paths from the vec of records.
  - `get_function`, `get_class` — the same.
  - `find_symbol` — the union of paths from functions+classes+variables+imports.
  - `get_imports` (by file and by module).
  - `get_file_summary` — path from args.
  - `get_callers`, `get_callees` — file ids from CallRecord.
  - `grep_body` — file_path directly from GrepBodyMatch.
  - `grep_code`, `grep_text`, `search_text` — path directly from the match structs.
  - `read_file` — path from args.
  - `list_files` — paths from ListedFile.
- **Support for `_meta.dependent_files` in BSL extension tools** (an empty array for now — the XML metadata parser is not tied to file_path; real dependencies are a task for the next iteration):
  - `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`.

### Changed

- **Workspace version** bumped 0.8.1 → 0.9.0 (minor — a backward-compatible format extension for the cache-ci client, breaking for clients that parsed the flat payload).

### Next steps

- Stage 3: `POST /invalidate {file_paths}` from the daemon to cache-ci after the SQLite `transaction.commit()` for a batch of FS events. The cache-ci 0.2.0 side is already ready to receive it.

## [0.8.1] — 2026-05-06

**Patch release: BSL extension tools in daemon mode and through federation.** It fixes two public regressions of v0.8.0 that made five BSL tools (`get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`) non-functional in the standard production scenario (repos served by the daemon, federation repos on a remote node).

### How we found it and why we fixed it ourselves

The regression was discovered by us **while operating v0.8.0** (2026-05-06): an attempt to call `get_object_structure` on any BSL repo led to `database error: no such table: metadata_objects`, and on a federation repo — to `extension tool '...' currently supports only local repos`. No one had reported the errors before us — external users of v0.8.0 may not have reached the 1C branch. Localized to two points in `code-index-core`: the calls to `apply_schema_extensions` / `index_extras` existed only in the CLI `index` command (`cli.rs`) and were absent in `daemon_core/worker.rs`; and `mcp::call_tool` had a hard rejection for `is_local == false`. After a full verification cycle (235 unit tests + a smoke on 4 BSL repos locally and through federation on the VM) — the fix was rolled out as the v0.8.1 patch without any involvement of the external community.

### Fixed

- **The daemon now applies the processors' `schema_extensions` and `index_extras`.** In v0.8.0 these calls were only in the CLI `index <path>` command, while the daemon worker did not make them. The result: on any BSL repo indexed via `bsl-indexer.exe daemon run`, the BSL tools failed with `database error: no such table: metadata_objects`. Now the `daemon_core/worker.rs` worker resolves the processor itself using the rule "explicit `language` from `daemon.toml` → fallback `detect()`", applies `apply_schema_extensions` BEFORE `full_reindex` (creates empty tables — the DDL is idempotent), and calls `index_extras` BEFORE `flush_to_disk` (populates the tables from `Configuration.xml`). For repos without a `Configuration.xml` (e.g., old data-processor dumps) the tables are created empty — the tools respond with `[]` and no exception.
- **Federation now forwards extension tools to remote nodes.** Previously any BSL-tool call on a remote repo (UT/BP_SS/BP_TDK/ZUP on the rag VM) returned `extension tool '...' currently supports only local repos`. A universal route `POST /federate/extension` was introduced with the payload `{tool_name, args}` — a single route for all extension tools, extensible when new LanguageProcessors are added. On the source side `mcp::call_tool` forwards the call through `dispatcher::dispatch_remote_value`. Both federation nodes must be upgraded to 0.8.1 synchronously — an old node will return 404 on the new route.

### Added

- **`ProcessorRegistry::resolve(explicit_language, repo_root)`** — a two-step processor resolution: first by the explicit `language` from the config, then a fallback to `detect()` by root markers. Used in the daemon worker and in the CLI `index` command. It unifies "indexing" behavior regardless of how it was launched.
- **The `mcp::ExtensionToolParams { tool_name, args }` struct** — the payload for the federation forward of extension tools.
- **Universal handler `handle_extension_tool` in `federation::server`** — finds the tool in the `extension_tools` snapshot, builds a `ToolContext` for a local repo, and calls `IndexTool::execute`. If there is no such tool on the target node (e.g., it was launched without bsl-extension) — it returns a `federation_error` with a clear message.

### Changed

- **`run_worker` takes `processor_registry: Option<Arc<ProcessorRegistry>>`** (the last parameter). `None` = universal-only build (`code-index.exe`); `Some(reg)` = `bsl-indexer.exe`. Used to resolve the processor of the current repo.
- **`runner::run` takes `processor_registry`** and threads it into `spawn_worker` (initial loop + `handle_reload`).
- **`cli::handle_daemon` takes `processor_registry`** — passed to `runner::run` when the daemon starts.
- **`Commands::Index` uses `resolve(None, root)`** instead of a direct `detect(root)` — identical behavior, but a single code path.

### Compatibility

The public API signature changes in `daemon_core::worker`/`runner`/`cli` are additive (new parameters at the end). The `bsl-indexer` 0.8.1 build is compatible with a v0.8.0 `daemon.toml` — no DB migration is needed (`apply_schema_extensions`'s DDL is idempotent).

**Federation:** both nodes must be upgraded at the same time. A pre-0.8.1 node will return `404 Not Found` on `POST /federate/extension`, and the new node will surface this as `federation_error`.

## [0.8.0] — 2026-05-05

**Phase 2 "content for code files"** — closing the main limitation of Phase 1. Before v0.8.0, `read_file` for `.py`/`.bsl`/`.rs`/`.ts` and other code files returned `category="code"` with an empty `content`. Now the content is stored in a new `file_contents` table (zstd compression, migration v4) and served on every call. Additionally: a new `grep_code` tool for regex search directly over code-file content, and an oversize mechanism for files larger than a configurable limit.

### Added

- **`file_contents` table (migration v4).** DDL: `file_contents(file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, content_blob BLOB, oversize INTEGER NOT NULL DEFAULT 0)`. Backfill is automatic — performed as part of `full_reindex` on the first run of v0.8.0 on an existing DB. Idempotent: a repeated call is safe (`INSERT OR REPLACE`). Estimate for UT (~15,665 `.bsl`, ~620 MB of sources): ~120 MB blob after zstd (~5×), a one-time backfill of ~1–2 minutes (pure I/O + zstd encode).

- **`read_file` works fully for code files.** For `.py`, `.bsl`, `.rs`, `.ts` and other AST languages the decompressed content from `file_contents` is returned. `category="code"`. The old logic for reading text files via `text_files` is unchanged.

- **Oversize mechanism.** Files larger than `max_code_file_size_bytes` (default **5 MB**) are stored with `oversize=1, content_blob=NULL`. AST parsing, FTS, and the call graph work fully for them. `read_file` for an oversize file returns a special response:
  ```json
  {
    "category": "code",
    "content": "",
    "oversize": true,
    "file_size": 8650240,
    "size_limit": null,
    "hint": "File is oversize: content is not stored in the index. Use get_function/get_class/grep_body."
  }
  ```

- **`stat_file` reports `oversize`** for code files: an `Option<bool>` field was added to the response. For text files it is always `null`.

- **The `max_code_file_size_bytes` limit configuration.** The hardcoded default is 5 MB (`DEFAULT_MAX_CODE_FILE_SIZE_BYTES` in `crate::daemon_core::config`). Overridden in `daemon.toml`:
  ```toml
  [indexer]
  max_code_file_size_bytes = 5242880   # global override (5 MB)

  [[paths]]
  path = "C:/RepoUT"
  max_code_file_size_bytes = 10485760  # for this repo — 10 MB
  ```
  Priority: per-path → the `[indexer]` section → the 5 MB default. The selection logic is the helper `PathEntry::effective_max_code_file_size(&IndexerSection)`.

- **New MCP tool `grep_code` (Phase 2 bonus).** Regex search over code-file content — it closes the blind spot of `grep_body` (which searches only in function/class bodies). The data source is the `file_contents` table (zstd-decode on the fly in Rust; SQL does a pre-filter by path/language). The parameters are identical to `grep_text`: `regex`, `path_glob?`, `language?`, `limit?`, `context_lines?`. Files with `oversize=1` are skipped. Storage method: `Storage::grep_code_filtered(regex, path_glob, language, limit, context_lines, max_total_bytes) -> Vec<GrepTextMatch>`. The pub function signature: `pub async fn grep_code(entry, regex, path_glob, language, limit, context_lines)`.

- **Federation route `/federate/grep_code`** — additive, does not break existing clients. A request to an old node (< 0.8.0) returns `404` — expected behavior; both nodes must be upgraded synchronously to use `grep_code` in federation.

### Changed

- **`Indexer::write_code_to_db`** — added a trailing parameter `raw_content: Option<&str>`. If set — the content is stored in `file_contents` (zstd encode). Internal API.
- **`Storage::read_file_text`** — added a trailing parameter `size_limit_bytes: Option<i64>`. Used to fill the `size_limit` field in the oversize response. The MCP layer passes `None`.
- **The `ParsedFile::Code` enum variant** — added a `raw_content: String` field.
- **`worker::run_worker`** — added an `IndexerSection` parameter (last). Inside, the effective limit is computed and written into `IndexConfig.max_code_file_size_bytes`.
- **`runner::spawn_worker`** — added an `IndexerSection` parameter, threaded into `run_worker`.

### Security

- **Protection against a zstd bomb.** All decompression calls in `read_file_content` and `grep_code_filtered` go through the private helper `Storage::decode_zstd_safe(blob) -> Result<Vec<u8>>`. It uses a streaming decoder with `io::Read::take(limit + 1)` — if the decompressed size exceeds `FILE_CONTENTS_MAX_DECOMPRESSED_BYTES` (256 MB), it returns an error and allocates no more RAM. 256 MB is well above any valid code file (5 MB default × ~5× zstd = ~25 MB; with headroom in case an operator raises `max_code_file_size_bytes`).

### Fixed

- **Backfill now works for all code files on a stable DB (a bug fix for the first preview build).** Previously the backfill was embedded in the processing of `metadata_updates` in `full_reindex` — a container of files with a changed mtime/file_size but the same content_hash. On a "stable" DB (nobody touched files since the last indexing) `metadata_updates` is empty, so the backfill **did not run for UT/BP_SS/ZUP** — only repos with actually changed files were populated (BP_TDK got ~15 files out of 90K). Fix: moved into a **separate phase** `Stage 6` after removing stale entries, via the new Storage method `list_code_files_without_content() -> Vec<(file_id, path)>`. Now the backfill hits all code files that have no record in `file_contents` AND no record in `text_files`, regardless of whether the mtime changed. Real figures on the rag VM after the fix: UT 32599/32599 in 31.7 s, BP_SS 37535/37535 in 37.9 s, ZUP 19066/19066 in 17.5 s, BP_TDK likewise.
- **Backfill in batches instead of one mega-transaction.** For a 90K-file repo, the whole phase inside a `BEGIN TRANSACTION` without a commit would bloat the WAL to many GB. An intermediate `commit_batch + begin_batch` every `batch_size.max(500)` files keeps the WAL within reasonable bounds.

### Compatibility

- **MCP API with no breaking changes.** All new response fields are `Option<...>` or `default false`; old clients will not break. The change to `read_file` for code files (returning real content instead of empty) is an improvement, not a breaking change.
- **DB schema** — migration v4 is idempotent and safe on an existing v0.7.x DB. Rolling back to v0.7.x simply ignores the new table — both versions are compatible for reading old data.
- **Storage API changed incompatibly** for direct users of the `code-index-core` crate: `Indexer::write_code_to_db`, `Storage::read_file_text`, `worker::run_worker`, `runner::spawn_worker` — new parameters. New public methods were also added: `Storage::upsert_file_content`, `read_file_content`, `has_file_content`, `delete_file_content`, `get_file_id_by_path`, `has_text_file`, `list_code_files_without_content`, `grep_code_filtered`. There are no external callers in the public API, but if there is private code with direct calls — update it.
- **Federation** — the new route `/federate/grep_code` is additive. **Both federation nodes must be upgraded synchronously** to use `grep_code` in federation (otherwise the old node returns 404 on this route). The general `v0.7.0+` principle remains.
- **`grep_code` skips oversize files** — this is a documented limitation, not a bug. For such files `get_function`/`get_class`/`grep_body` over AST data still work.

## [0.7.3] — 2026-05-04

**Bug fix**: extension tools (`get_object_structure`, `get_form_handlers`, and others provided via `LanguageProcessor::additional_tools()`) **were not registered in `tools/list`** when the server runs in federated mode (`serve.toml` present). For users in mono mode everything was correct.

### Fixed

- **`CodeIndexServer::from_federated`** now takes two extra parameters: `registry: Option<ProcessorRegistry>` and `local_languages: BTreeMap<String, String>`. The processor registry is stored in `Self.registry`, and right after building the repo map `extension_tools = collect_extension_tools(&active_languages, &reg)` is computed. Previously the federated constructor always initialized `extension_tools = Vec::new()` and `registry = None`, which zeroed out the conditional registration at serve start and on subsequent `reload_extensions` (`registry_opt = None` → `new_tools = Vec::new()`).
- **`local_languages` for federation**: the `alias → language` map is collected from the local `daemon.toml` (`PathEntry::effective_alias()` + `PathEntry.language`) and set into `RepoEntry.language` for **local repos**. Without this, `collect_active_languages` did not find bsl/python/rust in the federation scenario (`federation::repos::merge` returns a `FederatedRepo` without the language field). Remote repos via federation still arrive without a language — for them extension tools are registered only if the same language is active on a local repo on this node.
- **Behavioral consequence**: on the `bsl-indexer` build in federated mode, `tools/list` now returns 22 tools instead of 17 — `find_path`, `get_event_subscriptions`, `get_form_handlers`, `get_object_structure`, `search_terms` (the 5 BSL tools from `bsl-extension`) are added.

### Compatibility

- **MCP API unchanged** — the tool list changes only in the federated mode of the `bsl-indexer` build when there is at least one local repo with `language = "bsl"` in `daemon.toml`. The client sees this as a regular `notifications/tools/list_changed`.
- **DB schema with no migrations.**
- **Federation requires a synchronous upgrade of both nodes** — the general v0.7.0+ principle remains (the cross-node API did not change, but the useful effect is achieved only when both nodes are built at 0.7.3).
- The `from_federated` signature changed incompatibly. There are no external calls in the public code-index API (it was used only from `cli::run`), but if you have private code with a direct call — update it.

## [0.7.2] — 2026-04-29

**Bug fix to v0.7.1**: the HTML parser was not picked up in repos with an explicit `language="..."` (python/rust/bsl, etc.) in `daemon.toml`. An attempt to index `.html` files produced the error `No parser for extension: html`.

### Fixed

- **`ParserRegistry::from_languages`** now registers the HTML parser **always**, in addition to the specified `language`. HTML is a universal asset (templates, generated docs, sphinx output, vue/svelte SFCs, etc.) that occurs in repos of any "primary language" and is not listed separately in `daemon.toml`. The `"html" => …` branch in the `match` is kept as an explicit no-op for documentation; the actual registration happens after the `match`, unconditionally.
- This fixes the bug on `code-index index <repo> --force` for python/rust/bsl repos with html files.

### Compatibility

- MCP API unchanged.
- DB schema unchanged.
- A 0.7.1 binary without this fix may remain in production — html files simply will not get AST records until 0.7.2 + reindexing.

## [0.7.1] — 2026-04-28

**HTML parser** via tree-sitter — added **at a user's request**. Before 0.7.1, `.html` was indexed only as a text file (FTS+regex+read_file). Now it is a full AST with extraction of structural entities: elements with id, forms, input fields, links, inline scripts/styles, CSS classes. Backward compatibility is preserved: search_text/grep_text/read_file for html keep working via **dual indexing** (text_files + AST).

### Added

- **New parser** `crates/code-index-core/src/parser/html.rs` (~430 lines) based on `tree-sitter-html` 0.23. Supports `.html` and `.htm`. Registered in `ParserRegistry::new_all()` and `from_languages()`.
- **HTML semantics → code-index tables mapping:**

  | HTML construct | → | Table | Name |
  |---|---|---|---|
  | `<element id="X">…</element>` | `classes` | `X` (body=outerHTML, bases=tag_name) |
  | `<form id|name="X">` | `classes` | `form_X` (bases="form") |
  | `<form>` without id/name | `classes` | `form_<line>` |
  | `<input/select/textarea name="Y">` | `variables` | `Y` (value=type/value attribute) |
  | `<a href="URL">` | `imports` | `module=URL`, `kind="link"` |
  | `<link href="URL" rel="X">` | `imports` | `module=URL`, `kind=X` (or "stylesheet") |
  | `<script src="URL">` | `imports` | `module=URL`, `kind="script"` |
  | `<img/iframe/video/audio/source/embed src="URL">` | `imports` | `module=URL`, `kind=tag_name` |
  | `<script>…inline JS…</script>` | `functions` | `inline_script_<line>` (body=content) |
  | `<style>…inline CSS…</style>` | `functions` | `inline_style_<line>` (body=content) |
  | The `class="foo bar baz"` attribute | `variables` | `class:foo`, `class:bar`, `class:baz` (one record each) |

- **Dual indexing**: for languages from `is_dual_indexed_language()` (in 0.7.1 — only `html`), a record in `text_files` is created in parallel during indexing. This keeps `search_text`/`grep_text`/`read_file` working for HTML files alongside the new structured queries (`get_class("cart")`, `find_symbol("submitOrder")`, `get_imports(module="bootstrap.css")`, etc.). Implemented via a new field `text_for_fts: Option<String>` in `ParsedFile::Code` + an extra parameter `text_for_fts: Option<&str>` in `Indexer::write_code_to_db`.
- **File extensions**: `("html", "html")` and `("htm", "html")` moved from TEXT_EXTENSIONS to CODE_EXTENSIONS (`indexer/file_types.rs`). Added the public function `is_dual_indexed_language(language: &str) -> bool`.
- **13 unit tests** for the html parser (`parser/html.rs::tests`): id element, a form with id/name/without both, input/select/textarea, link/script/img imports, inline script, inline style, the classes attribute, tolerance to Jinja templates, empty HTML, nested elements. Plus `file_types::html_is_code_with_dual_indexing` to check the categorization.
- **Tolerance to templating engines**: `{{ … }}` and `{% … %}` are parsed as text content without crashing. Structural elements around them are extracted normally.

### Changed

- **`Indexer::write_code_to_db` signature**: added a trailing parameter `text_for_fts: Option<&str>`. An internal API, not MCP-visible. All known callers (worker.rs:380 for html, worker.rs:416 for xml_1c) are updated.

### Compatibility

- **MCP API unchanged** — no new tools, no new parameters. After reindexing, html files automatically become available to the existing tools: `get_class`, `find_symbol`, `search_function`, `get_imports`, `grep_body` + `search_text`, `grep_text`, `read_file`, `list_files`, `stat_file` keep working.
- **DB schema with no migrations.** The existing files / functions / classes / imports / variables / text_files tables are used. The dual insert for html goes through the former `insert_text_file`.
- **Federation with no new routes.** An internal mechanism; both nodes must be the same version (the 0.7.0 requirement still applies).
- **Reindexing:** on the first run of v0.7.1, the daemon finds the mtime of html files unchanged relative to the last indexing and **will not** reindex them (the mtime pre-filter from v0.4.0). To get new structured records for already-indexed html, you need either an explicit re-index (`code-index index <repo>`) or a change to the file mtime. Recommended on the first upgrade to 0.7.1 — a one-time full re-index of repos with html files.

## [0.7.0] — 2026-04-28

**Phase 1 "read-only tools"** — closing gaps in code-index so that a remote repo over federation works "like a local one" for most reconnaissance and reading tasks. A read-only release: the DB schema is untouched, no reindexing is needed, backward compatibility is preserved.

### Added

- **`stat_file(repo, path)`** — metadata of a single file from the `files` table. Returns `{exists, path, language, size, mtime, lines_total, content_hash, indexed_at, category}`. `category` — `"text"` (content available via `read_file`) or `"code"` (Phase 1 does not store content for AST languages).
- **`list_files(repo, pattern?, path_prefix?, language?, limit?)`** — a flat list of files with filtering. `pattern` — glob (`**/*.py`), `path_prefix` — a prefix (`src/auth/`). Returns `[{path, language, lines_total, size, mtime}]`. No separate `tree` endpoint — the structure is reconstructed from path strings.
- **`read_file(repo, path, line_start?, line_end?)`** — the content of a **text file** (yaml/md/json/toml/xml/sh/INI/CSV/SQL, etc.) via the `text_files` table. `line_start`/`line_end` are 1-based, inclusive. Soft-cap **5000 lines OR 500 KB** (whichever comes first) with a `truncated=true` flag. Hard-cap **2 MB** even with a range (rejection). For code files — `category="code"` and an empty `content` (to be closed in Phase 2). Returns `{content, lines_returned, lines_total, truncated, indexed_at, category}`.
- **`grep_text(repo, regex, path_glob?, language?, limit?, context_lines?)`** — regex search over text-file content via REGEXP. It closes the FTS5 gap with special characters (dots, parentheses, escapes). `path_glob` or `language` is desirable — otherwise it's a full scan, and the default limit is lowered to 100. `context_lines` — N lines before/after a match. A hard-cap on the total output size (1 MB).
- **`path_glob` parameter** in `search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`. It narrows the output by file path. Implementation — a post-filter via the `globset` crate after the SQL fetch. The SQL LIMIT is increased up to 5× (but no more than 500) so the filter does not leave an empty result.
- **`context_lines` parameter** in `grep_body` — N lines of context around the first up to 3 matches. Via the new `Storage::grep_body_with_options`. The existing `grep_body` without the context parameter works as before (backward compatibility for cli.rs/tests).
- **A hard-cap on the total response size** in `grep_body` (with context_lines) and `grep_text` — 1 MB. Protection against overflowing the model context on a wide regex with a large context_lines.
- **`Storage::get_path_by_file_id`** — a public method for the post-filter in the MCP layer.
- **`storage::normalize_glob`** (pub(crate)) — `**` → `*` for compatibility with the usual glob syntax (SQLite GLOB and `globset` already understand `*` as multi-char + `/`).
- **Federation routes:** `/federate/stat_file`, `/federate/list_files`, `/federate/read_file`, `/federate/grep_text`. Existing routes are extended with new parameters in the Params structs.
- **20 new unit tests** for Phase 1: `normalize_glob`, `slice_with_caps` (4 cases), `stat_file_meta` (3 cases), `list_files_filtered` (3 cases), `read_file_text` (4 cases), `grep_text_filtered` (3 cases), `grep_body_with_options`, `get_path_by_file_id`.

### Compatibility

- **MCP API with no breaking changes.** All new parameters are `Option<...>`, optional. Old clients unaware of `path_glob`/`context_lines` work as before.
- **Storage API with no breaking changes.** All existing methods (`search_functions`, `search_classes`, `search_text`, `grep_body`, `find_symbol`) kept their signature. New functionality is in new methods (`grep_body_with_options`) and in the post-filter in the MCP layer.
- **DB schema unchanged.** No migrations, no reindexing required.
- **Federation with no breaking changes.** New routes are additive. **Important:** both federation nodes (Windows and the VM) must be upgraded to 0.7.0 at the same time — otherwise calling new tools on an old node yields a 404.

### Known limitations of Phase 1

- **`read_file` for code files** (.py/.rs/.bsl/.ts/...) returns `category="code"` and an empty `content`. To be closed in Phase 2 with migration v4 + a zstd-compressed blob in the new `file_contents` table.
- **Files without an extension** (Dockerfile, Makefile, Jenkinsfile, .gitignore, LICENSE) are not indexed by the walker — a blind spot for DevOps repos. A deliberate limitation.
- **Binary 1C formats** (.epf, .erf, .cfe, .cf) are not indexed. Unpacking happens in an external pipeline.

## [0.6.1] — 2026-04-26

The rc7 technical debt is closed: a per-host port for the remote `code-index serve` used by federate forwarding. Up to and including 0.6.0 the remote node's port was hardcoded in `client.rs::DEFAULT_REMOTE_PORT = 8011`, and two serve nodes on the same machine inevitably overlapped in the connection pool — a pair was keyed only by IP. The change is fully backward compatible: a `serve.toml` without a `port` field works exactly as before (the default 8011 is used).

### Added

- **The `port: Option<u16>` field** in the `[[paths]]` section of `serve.toml` (`federation::config::ServePathEntry`). Optional, default — `DEFAULT_REMOTE_PORT` (8011). The `effective_port()` method returns the explicit value or the default. Validation forbids `port = 0` (reserved).
- **The `port: u16` field** in `federation::repos::FederatedRepo` and `mcp::RepoEntry` — mandatory, filled from `ServePathEntry::effective_port()` at `merge`. For local records the value is informational (forwarding is not used for them).
- **Tests:** `port_field_is_optional_and_defaults_to_remote_port`, `port_field_parses_when_explicit`, `zero_port_fails_validation` (config.rs), `port_defaults_when_not_set_and_propagates_when_set` (repos.rs), `pool_creates_separate_clients_for_different_ports_on_same_ip` (client.rs).

### Changed

- **`RemoteClientPool` keys clients by `(String, u16)`** instead of `String`. The signature is `get_or_create(&self, ip: &str, port: u16)`. The `default_port` field was removed: the pool itself does not fix a port; it is supplied per call from `RepoEntry::port`. `RemoteClientPool::new(timeout)` now takes only the timeout.
- **`dispatcher::dispatch_remote` and `dispatch_remote_value` take `port: u16`** between `ip` and `tool`. All 13 tool handlers (`mcp/mod.rs`) and `tools::remote_stats` are updated — they thread `entry.port`.

### Compatibility

- **A `serve.toml` without a `port` field** parses as before; `DEFAULT_REMOTE_PORT` is used for all records. No migrations are required.
- **The external MCP API is unchanged** — the `port` field does not appear in any tool call or tool result. It is a serve configuration detail and does not leave the process.
- **The caching proxy (planned)** will read `serve.toml` to determine which `port` to use for each repo — now this is a single source of truth.

## [0.6.0] — 2026-04-26

A large release: a workspace refactor, the new `bsl-indexer` binary with full 1C specificity, multi-config processing of a single repo with base/ + extensions/, parsing of `ConfigDumpInfo.xml` for debug UUID identifiers, optional LLM enrichment of procedures via the `enrichment` cargo feature, and protection against model drift via `embedding_signature`. All of it was done on the `workspace-refactor` branch (24+ commits, 249 tests).

### Added

- **Cargo Workspace**. The single mono-crate is split into 4 crates with clear areas of responsibility:
  - `code-index-core` (lib, publish=true) — the universal core: file scanner, tree-sitter parsers (Python/Rust/Go/Java/JS/TS/BSL), the SQLite schema, the MCP server, federation.
  - `code-index` (bin, publish=true) — the public binary without 1C specifics.
  - `bsl-extension` (lib, publish=false) — 1C specifics: XML parsers for the dump, the BSL call graph, the MCP tools `get_object_structure`/`get_form_handlers`/`get_event_subscriptions`/`find_path`/`search_terms`, optional LLM enrichment.
  - `bsl-indexer` (bin, publish=false) — the private binary = core + bsl-extension. Used on the rag VM for indexing 1C configurations.

- **Conditional MCP-tool registration**. At startup the MCP server reads `daemon.toml`, for each `[[paths]]` determines the `language` (explicitly or auto-detected by the repo root), collects the set of active languages, and registers ONLY the tools from matching `LanguageProcessor`s. If there is no BSL repository at all, the 1C tools do not appear in `tools/list` at all. A `notifications/tools/list_changed` notification is sent when `daemon.toml` is edited (file-watch with a 500ms debounce via `notify-debouncer-full`).

- **`bsl-indexer` — a new separate binary** for 1C configurations. The release CI builds it for Windows/Linux/macOS (with the `enrichment` feature for production). Detailed instructions — in [docs/bsl-indexer.md](docs/bsl-indexer.md); deployment to the rag VM — [docs/deploy-vm-rag.md](docs/deploy-vm-rag.md).

- **Multi-config layout** (`<repo>/base/Configuration.xml` + `<repo>/extensions/<EF_*>/Configuration.xml`). `BslLanguageProcessor::detects()` now recursively (depth ≤ 2) finds any `Configuration.xml`. `index_metadata_objects` traverses ALL configurations found in the tree and merges their objects into a single table (objects borrowed in extensions are skipped via `INSERT OR IGNORE`). `extension_name` is stored for each module — a filter between base and CFE is available via a query.

- **The `metadata_modules` table** with the UUID triple for the 1C platform debugger (`dbgs-debug` setBreakpoint):
  - `object_id` — the object/form UUID from the `uuid` attribute of the root element in its XML.
  - `property_id` — the UUID of the module type (Module/ManagerModule/FormModule/...) — a platform constant; the dictionary is in `module_constants.rs`.
  - `config_version` — a hash of the version from `ConfigDumpInfo.xml` (a separate parser). It changes on every configuration change.

  This triple lets agents set breakpoints by a human-readable module name without touching a live infobase. On the UT scale ~8K modules, on BP configurations ~10K.

- **MCP tool `search_terms`** — the third semantic search channel (after `search_function` and the future `semantic_search`). It uses FTS5 on the `procedure_enrichment.terms` column populated by LLM enrichment. Supports FTS syntax (AND, OR, NOT, "exact phrase", prefix*). NULL records (non-enriched procedures) are simply not found — this is progressive enhancement, not a bug.

- **The `bsl-indexer enrich [--path P] [--limit N] [--reenrich]` subcommand** under the `enrichment` cargo feature. An HTTP client to an OpenAI-compatible chat-completions endpoint (OpenRouter / Ollama / any compatible). Parallel processing via `tokio::task::JoinSet` with a configurable `batch_size`. Protection against model drift via `embedding_meta.enrichment_signature` — when the model in the config changes, a warning is printed suggesting `--reenrich`.

- **The `[enrichment]` section in `daemon.toml`** — provider, endpoint URL, model name, the name of the API-key env variable, batch size, the prompt template. Off by default (the feature is optional).

- **Language auto-detect with a write-back into `daemon.toml`** via `toml_edit` (preserves comments). Algorithm: `Configuration.xml` → bsl, `pyproject.toml`/`setup.py` → python, `Cargo.toml` → rust, `package.json` → javascript/typescript, otherwise by the prevailing extension. If the heuristic does not fire — a warning to the log and a skip (no silent fallback).

- **`Storage::apply_schema_extensions(extensions: &[&str])`** — the point of applying additional DDL from LanguageProcessors. Called once on the first open of a repo's DB for a language that needs specific tables.

- **`LanguageProcessor::index_extras(repo_root, &mut storage)`** — a hook for specific post-processing after the main indexing (e.g., parsing XML and populating the `metadata_*` tables). The default implementation is a no-op.

### Changed

- **A parallel run of 4 repos on the rag VM (8-core Intel Xeon)** — the total time of a full indexing of UT + BP_1 + BP_2 + ZUP dropped from ~8m30s (sequential) to **3m11s** (×2.7 speedup). The bottleneck is the single-thread SQLite FTS rebuild in each process; the disk (NVMe) does not block, and the cold↔warm cache difference is only ~5 s.

- **Protection against cascade transaction errors**. In each `index_*` function and in `build_call_graph` an idempotent `ROLLBACK` before `BEGIN` was added — if the previous function left an open transaction, the next one correctly closes it instead of crashing with "cannot start a transaction within a transaction".

- **`config_watch::run_watch` — an initial seeding of active_languages at startup**. Before the fix, a client connecting BEFORE the first file change saw only core tools (because in mono mode `RepoEntry.language=None` when loaded via `cli::run`). After the fix — the first `tools/list` immediately contains the correct set for the current `daemon.toml`.

- **CI setup**. `.github/workflows/release.yml` now builds 6 artifacts per tag: `code-index` × {Windows, Linux, macOS} + `bsl-indexer` × {Windows, Linux, macOS} (with `--features enrichment`). The cargo registry/git/target cache is keyed by `${{ runner.os }}-${{ matrix.target }}-${{ matrix.crate }}`.

### Security

- **`.mcp.json` excluded from tracking** via `.gitignore` + `git rm --cached`. The file is a local configuration; it contains SSH paths and URLs of a specific host and has no place in the repo.

- **Internal IPs replaced with RFC 5737 doc-IP** (`192.0.2.0/24`) in all federation tests, comments, and config examples. The specific rag VM addresses in the deployment instructions — with the placeholder `<vm-rag-ip>`.

### Empirical production verification (stages 7–8)

- **Conditional registration on Claude Code 2.1.120** — `tools/list` correctly contains 18 tools (5 BSL + 13 core) when there is a BSL repo in `daemon.toml`, and 13 tools (core only) without one.
- **`notifications/tools/list_changed` is IGNORED by Claude Code on 2.1.120** — the bug [anthropics/claude-code#13646](https://github.com/anthropics/claude-code/issues/13646) is confirmed empirically. The workaround is a manual `/mcp Reconnect`. Reconnect (issue #33779) on 2.1.120 already re-reads `tools/list` correctly.
- **The rag VM (Linux, 8 cores, NVMe)** — RepoUT 53.6 s cold cache, 57.7 s warm, a 5 s difference = the disk is not the bottleneck. A parallel indexing of all 4 repos in 3m11s on 8 cores × ~2 rayon cores per process.

### Documentation

- **[docs/bsl-indexer.md](docs/bsl-indexer.md)** — the user guide for `bsl-indexer`: what it can do, how to build with/without the `enrichment` feature, how to set up enrichment with OpenRouter / Ollama, and the MCP-client limitations with a workaround.
- **[docs/bsl-indexer-architecture.md](docs/bsl-indexer-architecture.md)** — the full architectural spec of the workspace refactor with the rationale for decisions.
- **[docs/deploy-vm-rag.md](docs/deploy-vm-rag.md)** — a step-by-step deployment guide for the VM (installing the Rust toolchain, copying the sources, configuring daemon.toml, the systemd unit, the A/B protocol for comparison with pg_indexer).
- **[deploy/systemd/bsl-indexer-daemon.service](deploy/systemd/bsl-indexer-daemon.service)** — a ready systemd unit with resource limits and protection against writing outside the allowed directories.

## [0.5.0-rc6] — 2026-04-25

### Added

- **Federated `code-index serve` architecture** (modeled on `1c-router`/`mcp__1c__`). A single serve process serves a registry of repositories from several machines: for each tool call with `repo=X` the local serve looks at the ip — if it matches `[me].ip`, it reads the local SQLite, otherwise it makes an HTTP call to the remote serve. The source of truth for each repo is on a single machine (this is a proxy, not replication).

  **New config** [`serve.toml`](src/federation/config.rs) — global, identical on all nodes (rolled out via a shared git repo `code-index-config`):

  ```toml
  [me]
  ip = "192.0.2.10"
  # token = "..."   # optional, not validated in rc6 (a stub for rc7)

  [[paths]]
  alias = "ut"
  ip = "192.0.2.50"

  [[paths]]
  alias = "dev"
  ip = "192.0.2.10"
  ```

  `daemon.toml` stays local (only this machine's paths, no schema changes).

- **An internal endpoint `POST /federate/<tool_name>`** ([`src/federation/server.rs`](src/federation/server.rs)) — the receiving side of forwarding. The request body is JSON matching our `*Params` structs exactly. The response is whatever the local tool handler would have returned. `/federate` lives on the same axum router as `/mcp` and is protected by a shared whitelist middleware.

- **IP whitelist middleware** ([`src/federation/whitelist.rs`](src/federation/whitelist.rs)). serve binds to `[me].ip` (not to `127.0.0.1`, not to `0.0.0.0`) — the port is active only on one interface. The allowed peer IPs are from `{all [[paths]].ip} ∪ {127.0.0.1, ::1}`. A foreign peer → `403 {"error":"forbidden","peer":"..."}`.

- **A parallel fan-out in `get_stats(repo=None)`** ([`src/mcp/tools.rs`](src/mcp/tools.rs)) via `tokio::task::JoinSet`. Each remote repo is polled with a 5 s timeout; unreachable ones are returned as `{"repo":"...","status":"unreachable","error":"..."}` without blocking the rest.

- **The `--serve-config <FILE>` flag on `code-index serve`**. If the flag is not set — `$CODE_INDEX_HOME/serve.toml` is searched. If there is no file — serve works as rc5 (mono mode, bind 127.0.0.1, no whitelist). With `transport=stdio` or an explicit `--path`, federation is not activated.

  ```bash
  # Federated mode (rc6+):
  code-index serve --transport http --port 8011

  # Compatible rc5 mode (mono):
  code-index serve --transport http --port 8011 --path ut=C:/RepoUT
  ```

- **A pool of reusable HTTP clients** ([`src/federation/client.rs`](src/federation/client.rs)) — one `reqwest::Client` per remote IP, lazy init via `RemoteClientPool::get_or_create`. Timeout 5 s; idle pool 60 s.

### Changed

- **`RepoEntry` now stores `ip` and `is_local`**, and the `root_path` and `storage` fields are wrapped in `Option` (`None` for remote). The old constructors `open_readonly_multi` / `open_readonly` / `with_storage` set `is_local=true`, `ip="127.0.0.1"` — backward compatibility for tests and mono mode.

- **`serve_http` takes optional `federate_router` and `whitelist`**. If passed — `Router::merge` for `/federate/*` and `axum::middleware::from_fn_with_state` for the whitelist. The listener now uses `into_make_service_with_connect_info::<SocketAddr>()` — without it the peer IP is not extracted in the middleware.

- **`--host` became `Option<String>`**. If set — CLI takes priority; otherwise, if serve.toml is present — `[me].ip`, otherwise `127.0.0.1` (the rc5 default).

### Loop protection

- **No headers** like `X-Forwarded-Already`. Protection is static, via the config: each node knows its own `[me].ip` and forwards only if `repo.ip != own_ip`. On a config mismatch (`A: X→B`, `B: X→A`) the request fails by the 5s timeout with a clear error.
- The `/federate/get_stats` receiver without `repo` limits the fan-out to its own local repos (it does not recursively traverse to others) to exclude a loop between nodes.

### Roadmap (outside rc6)

- Creating the `code-index-config` git repo with a `serve.toml` template — an operational task.
- A Linux binary + a systemd unit for deployment to VM 200.
- `[me].token` authorization — a Bearer header in `/federate/*`, checked in the whitelist middleware. The field is already parsed in the serve.toml schema.
- A HEAD ping to the remote nodes in `health` — a low-priority feature.
- Hot-reload of `serve.toml` without a restart (`POST /reload` for serve).

## [0.5.0-rc5] — 2026-04-22

### Added

- **HTTP transport on `code-index serve`** via rmcp's `StreamableHttpService`. A single process serves all repositories under `mcp-supervisor`; clients connect to a shared URL without copying `--path` into each `.mcp.json`.

  ```bash
  # stdio (per-session, as before)
  code-index serve --path ut=C:/RepoUT --path bp=C:/RepoBP

  # http (shared process)
  code-index serve --transport http --port 8011 --config C:/tools/code-index/daemon.toml
  ```

  The client `.mcp.json`:
  ```json
  "code-index": { "type": "http", "url": "http://127.0.0.1:8011/mcp" }
  ```

  Implementation — [`src/main.rs`](src/main.rs) `serve_http`: `StreamableHttpService::new(factory, LocalSessionManager, StreamableHttpServerConfig::default())`, mounted into `axum::Router::nest_service("/mcp", svc)`. The factory clones the already-built `CodeIndexServer` (it is `Clone`), so all sessions share a common set of open SQLite databases.

- **Multi-repo in a single serve process**. `--path` now takes `alias=dir` and may be specified multiple times — each tool call passes a `repo=<alias>` parameter to select the repository. Without `=` — the old `alias=default` contract. The tool parameters got a `repo: String` field; the internal `RepoEntry` struct holds an open read-only `Storage` and `root_path` per repo.

- **The `alias` field in `[[paths]]` of daemon.toml** — [`src/daemon_core/config.rs`](src/daemon_core/config.rs) `PathEntry::alias: Option<String>`. If not set — the alias is computed via `PathEntry::effective_alias()` from the last path segment (lowercase, spaces → `_`). The daemon ignores the field; only `code-index serve --config ...` uses it when building the repo list. Old configs without `alias` keep working (`#[serde(default)]`).

- **The `--host`, `--port`, `--config` flags on `serve`**. `--config` points at `daemon.toml` — the list of repos and aliases is taken from there. CLI `--path` takes priority over the config. The default port is 8011 (the next free one in the mcp-supervisor range: 8001/8002/8007/8010).

### Dependencies

- Enabled the `rmcp/transport-streamable-http-server` feature (it pulled in `transport-streamable-http-server-session`, `server-side-http`, and transitively — `uuid`, `sse-stream`). `axum` and `tower` were already in deps for the daemon's health endpoint.

## [0.5.0-rc4] — 2026-04-17

### Fixed

- **The daemon crashed when the console was closed on Windows**. `code-index` is built as a console-subsystem application: when launched in a user session (a Scheduled Task with `LogonType=Interactive`, a manual call from `cmd`/PowerShell), the process gets a console window and becomes its child. Closing the window sends `CTRL_CLOSE_EVENT`, and the daemon dies with it. For the standard installation via `scripts/install-daemon-autostart.ps1` this meant the console window popped up at logon, and closing it stopped the indexing.

  **Fix**: in [`src/main.rs`](src/main.rs), `handle_daemon` for `daemon run` on Windows performs a self-detach — it restarts itself with the `DETACHED_PROCESS | CREATE_NO_WINDOW` flags, sets the environment variable `CODE_INDEX_DAEMON_DETACHED=1`, and terminates the parent process. The detached clone runs without a console and survives the closing of any parent session. On Unix the self-detach is not performed — daemonization is managed by `systemd`/`launchd`.

  The implementation uses only `std::os::windows::process::CommandExt::creation_flags` and adds no new dependencies.

## [0.5.0-rc3] — 2026-04-17

### Fixed

- **A race condition on editors' atomic save**. Editors (VS Code, IDEs, `git`) save files atomically: first they write to a temporary `<name>.tmp.<pid>.<ts>`, then rename it to the target file. The watcher via `ReadDirectoryChangesW` managed to see a `Create` event on the `.tmp` file, but by the time `hasher::file_hash()` was called the file had already been renamed. A wall of errors poured into the logs of the form `file_hash \\?\...\.mcp.json.tmp.10296.1776427368309: The system cannot find the file specified. (os error 2)`.

  **Fix**: in [`daemon_core/worker.rs`](src/daemon_core/worker.rs), `apply_event` on `io::ErrorKind::NotFound` from `file_hash` now silently exits the handler. Only real errors are logged (permission denied, read error, etc.).

### Added

- **The `exclude_file_patterns` field in `.code-index/config.json`** — glob patterns of file names to exclude from indexing. It complements the existing `exclude_dirs`:

  ```json
  {
    "exclude_dirs": [".vscode", "experimental"],
    "exclude_file_patterns": ["*.tmp.*", "*.bak", "*.orig", "*.swp"]
  }
  ```

  Patterns are matched by **basename** (the file name without the path). They are applied:
  - in [`watcher.rs`](src/watcher.rs) — events from files matching a pattern are discarded before the `file_hash` call;
  - in [`indexer/mod.rs`](src/indexer/mod.rs) `collect_candidates` / `collect_candidates_standalone` — the file is excluded from the WalkDir traversal before categorization.

  The glob syntax is via the `globset` crate. Invalid patterns are logged and skipped (they do not break the startup).

### Dependencies

- Added `globset = "0.4"`.

## [0.5.0-rc2] — 2026-04-17

### Fixed

- **WAL files grew to tens of GB in production**. After a day of work on our stand with 13 indexed folders (5 large 1C repos + 8 MCP modules) the WAL files took ~43 GB while the total size of the main DBs was ~16 GB:

  | Repo | `index.db` | `index.db-wal` (before the fix) |
  |------|-----------|---------------------------|
  | RepoBP_2 | 4.7 GB | **19 GB** |
  | RepoUT | 2.1 GB | **17 GB** |
  | RepoZUP | 5.1 GB | 5.1 GB |
  | dbgs-debug | 1.4 GB | 1.4 GB |

  Free space on the system drive shrank by ~45 GB in a day.

  **The cause**, found by code analysis:
  1. `PRAGMA wal_autocheckpoint=500` (added in v0.5.0-rc1) moves pages from the WAL into the main DB, but **does not reduce the physical WAL file** — only an explicit `PRAGMA wal_checkpoint(TRUNCATE)` does that.
  2. Under a bulk load (the initial reindex of 90K files, frequent watcher batches) the checkpoint does not keep up with the write rate.
  3. The worker called `Storage::flush_to_disk()` via `Connection::backup()` after every batch — in disk mode (and the worker is always in it after a reopen) this is a useless copy of the DB onto itself, and the WAL does not shrink.

  **Fix**:
  - Added a `Storage::checkpoint_truncate()` method — a wrapper over `PRAGMA wal_checkpoint(TRUNCATE)` that actually collapses the WAL.
  - In `worker.rs` after the initial reindex (when the worker is guaranteed to be in disk mode) — a mandatory `checkpoint_truncate`. This is the "fattest" source of WAL.
  - In the watcher loop after `commit_batch` — `flush_to_disk` replaced with `checkpoint_truncate`. On graceful shutdown — the same.

  **The result of the check on the same 13 folders**: the WAL stays at **0 bytes** after the initial reindex and after file edits through the watcher. ~48 GB freed.

## [0.5.0-rc1] — 2026-04-17

A major rework of the architecture: splitting into a **background writer daemon** and **MCP readers**.

### Breaking changes

- **`code-index serve` is now read-only**. It no longer indexes and does not hold a watcher — it only connects to the DB maintained by a separate daemon. If the daemon is not running or the folder is not in its config, a tool call returns a structured response `{"status": "daemon_offline" | "not_started" | "indexing", ...}` rather than crashing.
- **The per-project PID lock was removed** (the `.code-index/serve.pid` file is no longer created). Any number of MCP processes can connect to a single `.code-index/index.db` in parallel.
- **The `--no-watch`, `--flush-interval` flags** on `serve` were removed — they were specific to the former writer role and are inapplicable to read-only.

### Added

- **The `daemon` subcommand**: `code-index daemon run/start/stop/status/reload`. `run` — foreground (for a Scheduled Task / systemd), `start`/`stop`/`status`/`reload` — an HTTP client to a running daemon.
- **The `CODE_INDEX_HOME` environment variable** — a single point of configuration. It contains `daemon.toml`, and the runtime files `daemon.pid`, `daemon.json`, `daemon.log` are placed there too. Works both via a system variable (`setx`) and via an `"env"` block in `.mcp.json`.
- **The `daemon.toml` config** with the list of watched folders and parameters:
  - `max_concurrent_initial` — how many folders are in the initial-reindex phase at once (default `1`, protection against a RAM spike).
  - Per-folder `debounce_ms` / `batch_ms` — overriding the watcher delay per project.
- **HTTP health IPC on loopback**: `GET /health`, `GET /path-status?path=...`, `POST /reload`, `POST /stop`. The port is chosen automatically and written into `daemon.json`.
- **A per-folder lifecycle**: `not_started → initial_indexing → ready ⇄ reindexing_batch | error`. Visible in `daemon status`.
- **A PowerShell script** `scripts/install-daemon-autostart.ps1` to install a Scheduled Task (the trigger is the user logon; it automatically runs `setx CODE_INDEX_HOME`).

### Changed

- **Memory**: only one in-memory SQLite storage at a time. After the initial reindex the worker flushes → reopens the same file in disk mode (WAL) → releases the semaphore permit. Peak RAM does not sum across folders.
- **Repeated startup**: if `.code-index/index.db` already exists, the worker opens it directly in disk mode (skipping the backup disk→memory→disk). On 2 1C repos of ~90K files each, a repeated start takes **~12 s** (previously ~600 s with the same code before the fix).
- **SQLite**: added `PRAGMA wal_autocheckpoint=500` and `PRAGMA journal_size_limit=67108864` — the WAL file does not bloat over long transactions and is truncated to 64 MB after a checkpoint.
- **The MCP server** checks the folder status at the daemon before each tool call. If the folder is not `ready` — it returns a structured JSON with progress/a hint rather than an empirical result from a stale index.

### Removed

- The legacy modules `src/daemon.rs` and `src/pidlock.rs`.

### Measurements (1C repos, 2 folders of 88–92K files, 80% XML)

| Scenario | Time |
|----------|-------|
| Initial reindex from scratch, both folders sequentially (`max_concurrent_initial=1`) | ~10 min, RAM peak ~6 GB |
| Repeated start on an existing DB | ~12 s |
| Watcher: from a file edit to its appearance in the index | ~1.6 s (of which 1.5 s is the debounce — configurable) |
| Graceful shutdown (`daemon stop`) | DB on disk without `-wal`/`-shm` files |

### Technical debt

- A `0/0` progress in `daemon status` during the initial reindex (cosmetic — it is not updated from the blocking phase).
- Linux / macOS are not verified live — there is only theoretical cross-platform support via the `dirs` and `notify` crates. Feedback on the first non-Windows runs is appreciated.
- There are no integration tests for daemon_core — only unit tests for `config`, `ipc`, `state`.

## [0.4.0] — 2026-03-30

- An `mtime`+`file_size` pre-filter for the initial reindex: 93K files are re-checked in ~4 s instead of ~163 s (SHA-256 only for changed files).
- The `migrate_v3` migration — adds the `mtime`, `file_size` columns to the `files` table.
- A per-project PID lock (`.code-index/serve.pid`) — protection against running two `serve` instances on one project simultaneously.

## [0.3.0] — 2026-03-...

- Parallel read+hash via rayon.
- `hash_ast` without `to_sexp` (faster).
- Removal of `max_file_size` for code — large BSL/XML files are now indexed in full.
- Tuning of `mmap_size`, `batch_size` for the initial indexation.
