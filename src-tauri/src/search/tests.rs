    use super::chat::CHAT_DIGEST_MAX_CHARS;
    use super::enrich::enrich_hits;
    use super::fts::fts_search;
    use super::service::{global_search_with, search_chats_with, search_project, search_with};
    use super::text::{fallback_snippet, fts_match_query, tokenize};
    use super::types::{GlobalHit, SearchHit};
    use crate::db;
    use crate::embed;
    use crate::error::Result;
    use crate::index;
    use crate::paths;
    use crate::state::AppState;
    use rusqlite::{params, Connection};
    use crate::config::IndexingConfig;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::tasks;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn setup(tag: &str) -> (PathBuf, Connection, i64, AppState) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        (wd, conn, dto.id, state)
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
    }

    // Deterministic stand-in: byte-bucket counts, no network.
    struct FakeEmbedder;

    impl embed::Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 768];
                    for b in t.bytes() {
                        v[b as usize % 768] += 1.0;
                    }
                    v
                })
                .collect())
        }
        fn dim(&self) -> usize {
            embed::EMBED_DIM
        }
        fn backend_id(&self) -> &'static str {
            "fake"
        }
    }

    fn rels(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.rel_path.clone()).collect()
    }

    #[test]
    fn fts_only_finds_cyrillic_word_and_snippet_wraps_match() {
        let (wd, _conn, id, state) = setup("search-fts");
        fs::create_dir_all(root(&wd).join("notes")).unwrap();
        fs::write(
            root(&wd).join("notes/записи.md"),
            "Клиент example.ru оплатил счёт за редизайн",
        )
        .unwrap();
        index::index_project(&state, id).unwrap(); // default config: noop embedder

        let hits = search_project(&state, id, "оплатил", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.rel_path, "notes/записи.md");
        assert_eq!(hit.name, "записи.md");
        assert_eq!(hit.kind, "file");
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("<<оплатил>>"), "{:?}", hit.snippet);
        assert!(hit.modified > 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fts_injection_attempts_are_treated_as_plain_tokens() {
        let (wd, _conn, id, state) = setup("search-inject");
        fs::write(root(&wd).join("near.md"), "near or far").unwrap();
        fs::write(root(&wd).join("only.md"), "near here").unwrap();
        fs::write(root(&wd).join("foo.md"), "foo bar").unwrap();
        index::index_project(&state, id).unwrap();

        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced", "NEAR", "foo*"] {
            assert!(
                search_project(&state, id, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        let query = |q: &str| rels(&search_project(&state, id, q, 10).unwrap());
        // operators become plain words: AND of both tokens, not an OR operator
        assert_eq!(query("NEAR OR"), ["near.md"]);
        assert_eq!(query("near OR"), ["near.md"]);
        assert_eq!(query("near OR foo"), Vec::<String>::new());
        // pure syntax yields no tokens, hence no results
        assert!(query("*").is_empty());
        assert!(query("\"").is_empty());
        assert!(query("\"unbalanced").is_empty());
        // "*" is stripped, the plain token still matches
        assert_eq!(query("foo"), ["foo.md"]);
        assert_eq!(query("foo*"), ["foo.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn hybrid_adds_semantic_only_hit_and_merges_match_types() {
        let (wd, conn, id, _state) = setup("search-hybrid");
        fs::write(root(&wd).join("кошки.md"), "кошки кошки кошки").unwrap();
        fs::write(root(&wd).join("самолет.md"), "самолет летит над морем").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        // differently worded query: FTS finds nothing, semantic finds the file
        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["semantic"]);
        assert!(!hits[0].snippet.is_empty());

        // literal word: both legs hit the same file, both matchTypes kept
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["fts", "semantic"]);

        // RRF ordering is deterministic across runs
        let again = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(rels(&hits), rels(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn search_with_runs_every_leg_on_the_passed_connection() {
        // S1.2: one connection per call. File FTS, file semantic, task FTS,
        // task semantic, mtime and snippet legs must all work through the
        // passed conn without opening another one.
        let (wd, conn, id, _state) = setup("search-one-conn");
        fs::write(root(&wd).join("кошки.md"), "кошки кошки кошки").unwrap();
        tasks::task_create(&conn, id, "кошки в задаче", None).unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        crate::db::open_counter::arm();
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        let opens = crate::db::open_counter::stop();
        assert_eq!(opens, 0, "search legs must not open connections");

        let file = hits.iter().find(|h| h.kind == "file").expect("file hit");
        assert_eq!(file.match_types, ["fts", "semantic"]);
        assert!(file.modified > 0, "mtime leg must use the passed conn");
        let task = hits.iter().find(|h| h.kind == "task").expect("task hit");
        assert_eq!(task.match_types, ["fts", "semantic"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn enrichment_carries_per_hit_mtime_and_fallback_snippet() {
        // S1.3: the batched enrichment must yield exactly the values the old
        // per-hit lookups did: own mtime row per hit, FTS snippet kept,
        // semantic-only hits get a fallback snippet from stored content.
        let (wd, conn, id, _state) = setup("search-enrich-values");
        fs::write(root(&wd).join("а.md"), "слово в файле а").unwrap();
        fs::write(root(&wd).join("б.md"), "слово в файле б").unwrap();
        fs::write(root(&wd).join("в.md"), "кошки кошки кошки").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();
        for (rel, m) in [("а.md", 111), ("б.md", 222), ("в.md", 333)] {
            conn.execute(
                "UPDATE indexed_files SET mtime = ?1 WHERE rel_path = ?2",
                params![m, rel],
            )
            .unwrap();
        }

        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        let by_rel = |rel: &str| hits.iter().find(|h| h.rel_path == rel).expect(rel);
        for (rel, m) in [("а.md", 111), ("б.md", 222)] {
            let hit = by_rel(rel);
            assert_eq!(hit.modified, m, "{rel} must carry its own mtime");
            assert!(hit.snippet.contains("<<слово>>"), "{rel}: {:?}", hit.snippet);
        }
        let sem = by_rel("в.md");
        assert_eq!(sem.match_types, ["semantic"]);
        assert_eq!(sem.modified, 333);
        assert!(sem.snippet.contains("кошки"), "{:?}", sem.snippet);
        // A file with no indexed row keeps the old fallbacks: mtime 0.
        conn.execute("DELETE FROM indexed_files WHERE rel_path = 'а.md'", [])
            .unwrap();
        let hits2 = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        let stale = hits2.iter().find(|h| h.rel_path == "а.md").expect("а.md");
        assert_eq!(stale.modified, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn enrichment_uses_batched_queries_instead_of_one_per_hit() {
        // S1.3: statement trace must show exactly one mtime and one content
        // batch query for the whole call, no per-hit single-row lookups.
        let (wd, conn, id, _state) = setup("search-enrich-count");
        for name in ["а.md", "б.md", "в.md", "г.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        fs::write(root(&wd).join("sem.md"), "кошки кошки кошки").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        thread_local! {
            static BATCHED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn count_batched(event: rusqlite::trace::TraceEvent<'_>) {
            let sql = match event {
                rusqlite::trace::TraceEvent::Stmt(_, sql) => sql,
                _ => return,
            };
            if sql.contains("rel_path IN (") {
                BATCHED.with(|c| c.set(c.get() + 1));
            }
        }
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_batched),
        );
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);

        assert!(hits.len() >= 5, "expected several file hits: {hits:?}");
        assert_eq!(
            BATCHED.with(|c| c.get()),
            2,
            "one mtime + one content batch query for {} hits",
            hits.len(),
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    // Representative cross-project query on the demo working dir
    // (~/Documents/PROJECT-S, PROJECT_S_WD override; indexed demo data and a
    // provisioned model). "Before" = crm_search's old shape: search_project
    // per project, each opening its own connection; "after" = one connection
    // and one embedder select for the whole call:
    //   cargo test --release search_conn_timing -- --ignored -- --nocapture
    #[test]
    #[ignore]
    fn search_conn_timing_one_connection_per_call() {
        use std::time::Instant;
        let wd = crate::embed::model_store::real_working_dir();
        let db_file = paths::db_path(&wd);
        assert!(db_file.exists(), "demo DB not found at {}", db_file.display());
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        let pids: Vec<i64> = {
            let conn = db::open(&db_file).unwrap();
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(!pids.is_empty(), "demo DB has no projects");
        let query = "редизайн";
        let limit = 10usize;
        // Warm the embedder cache so ONNX session load is not timed.
        search_project(&state, pids[0], query, 1).unwrap();

        let mut runs = Vec::new();
        for round in 1..=3 {
            let started = Instant::now();
            let old_hits: usize = pids
                .iter()
                .map(|pid| search_project(&state, *pid, query, limit).unwrap().len())
                .sum();
            let old_ms = started.elapsed().as_secs_f64() * 1000.0;

            let started = Instant::now();
            let conn = db::open(&db_file).unwrap();
            let embedder = embed::select_embedder(&wd);
            let new_hits: usize = pids
                .iter()
                .map(|pid| {
                    search_with(&conn, embedder.as_ref(), embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let new_ms = started.elapsed().as_secs_f64() * 1000.0;

            // The slice's actual saving: N opens (one per project) vs one open
            // for the whole call. The hybrid total is embedder-dominated, so
            // the FTS-only path (noop embedder: offline default and the
            // semantic-skipped case) is timed separately — there the per-call
            // open dominates.
            let started = Instant::now();
            let fts_old_hits: usize = pids
                .iter()
                .map(|pid| {
                    let conn = db::open(&db_file).unwrap();
                    search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let fts_old_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            let conn = db::open(&db_file).unwrap();
            let fts_new_hits: usize = pids
                .iter()
                .map(|pid| {
                    search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let fts_new_ms = started.elapsed().as_secs_f64() * 1000.0;

            assert_eq!(old_hits, new_hits, "round {round}: hits changed");
            assert_eq!(fts_old_hits, fts_new_hits, "round {round}: FTS hits changed");
            eprintln!(
                "search conn timing #{round}/3: hybrid cross-project call \
                 per-project-open={old_ms:.1} ms, one-open={new_ms:.1} ms \
                 ({new_hits} hits, {} projects); FTS-only per-project-open={fts_old_ms:.2} ms, \
                 one-open={fts_new_ms:.2} ms ({fts_new_hits} hits)",
                pids.len(),
            );
            runs.push((fts_old_ms, fts_new_ms));
        }
        assert!(runs.iter().all(|(many, one)| many > one));
    }

    // Representative FTS-only query on the demo working dir (S1.3). Two
    // measurements, 3 rounds each: the whole cross-project call (end-to-end,
    // noise-dominated at demo scale) and the enrichment stage in isolation —
    // the old per-hit point queries vs the batched enrich_hits. Enrichment is
    // idempotent, so both shapes run over the same already-enriched hits and
    // measure the same query work. The digest (rel_path|modified|snippet over
    // all hits, FNV-1a) must match between the two shapes:
    //   cargo test --release search_batch_enrich_timing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn search_batch_enrich_timing_fts_only() {
        use std::time::Instant;
        let wd = crate::embed::model_store::real_working_dir();
        let db_file = paths::db_path(&wd);
        assert!(db_file.exists(), "demo DB not found at {}", db_file.display());
        let conn = db::open(&db_file).unwrap();
        let pids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(!pids.is_empty(), "demo DB has no projects");
        // "example" matches 29 of the 85 demo files: near-limit hit counts per
        // project, the worst realistic N+1 case on this DB.
        let query = "example";
        let limit = 10usize;
        let lowered: Vec<String> = tokenize(query).iter().map(|t| t.to_lowercase()).collect();
        search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, pids[0], query, limit).unwrap();

        // The per-hit shape this slice removes, kept here only to time it.
        fn old_file_mtime(conn: &Connection, project_id: i64, rel: &str) -> i64 {
            conn.query_row(
                "SELECT mtime FROM indexed_files WHERE project_id = ?1 AND rel_path = ?2",
                rusqlite::params![project_id, rel],
                |r| r.get(0),
            )
            .unwrap_or(0)
        }
        fn old_file_content(conn: &Connection, project_id: i64, rel: &str) -> Option<String> {
            conn.query_row(
                "SELECT content FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
                rusqlite::params![project_id.to_string(), rel],
                |r| r.get(0),
            )
            .ok()
        }
        let digest = |sets: &[(i64, Vec<SearchHit>)]| -> u64 {
            let mut hash: u64 = 0xcbf29ce484222325;
            for (_, hits) in sets {
                for h in hits {
                    for b in format!("{}|{}|{}\u{1}", h.rel_path, h.modified, h.snippet).bytes() {
                        hash ^= b as u64;
                        hash = hash.wrapping_mul(0x100000001b3);
                    }
                }
            }
            hash
        };

        let mut total_ms = 0.0;
        let mut call_digests = Vec::new();
        for round in 1..=3 {
            let started = Instant::now();
            let per_pid: Vec<(i64, Vec<SearchHit>)> = pids
                .iter()
                .map(|pid| {
                    (
                        *pid,
                        search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                            .unwrap(),
                    )
                })
                .collect();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            total_ms += ms;
            call_digests.push(digest(&per_pid));
            let hits: usize = per_pid.iter().map(|(_, h)| h.len()).sum();
            eprintln!(
                "search enrich timing #{round}/3: FTS-only cross-project call = {ms:.2} ms \
                 ({hits} hits, {} projects), digest = {:016x}",
                pids.len(),
                call_digests[round - 1],
            );

            let started = Instant::now();
            let mut old_set = per_pid.clone();
            for (pid, hits) in &mut old_set {
                for hit in hits.iter_mut() {
                    hit.modified = old_file_mtime(&conn, *pid, &hit.rel_path);
                    if hit.snippet.is_empty() {
                        let content = old_file_content(&conn, *pid, &hit.rel_path).unwrap_or_default();
                        hit.snippet = fallback_snippet(&content, &lowered);
                    }
                }
            }
            let old_ms = started.elapsed().as_secs_f64() * 1000.0;

            let started = Instant::now();
            let mut new_set = per_pid.clone();
            for (pid, hits) in &mut new_set {
                enrich_hits(&conn, *pid, hits, &lowered).unwrap();
            }
            let new_ms = started.elapsed().as_secs_f64() * 1000.0;

            assert_eq!(
                digest(&old_set),
                digest(&new_set),
                "round {round}: enrichment values changed"
            );
            eprintln!(
                "search enrich stage #{round}/3: per-hit queries = {old_ms:.3} ms, \
                 batched = {new_ms:.3} ms ({hits} hits)"
            );
            assert!(new_ms < old_ms, "round {round}: batching must be faster");
        }
        assert!(call_digests.windows(2).all(|w| w[0] == w[1]), "digest unstable");
        eprintln!(
            "search enrich timing: call mean = {:.2} ms, digest = {:016x}",
            total_ms / 3.0,
            call_digests[0],
        );
    }

    #[test]
    fn empty_query_returns_empty_and_noop_backend_stays_fts_only() {
        let (wd, _conn, id, state) = setup("search-empty");
        fs::write(root(&wd).join("a.txt"), "кошки").unwrap();
        index::index_project(&state, id).unwrap();

        assert!(search_project(&state, id, "", 10).unwrap().is_empty());
        assert!(search_project(&state, id, "   ", 10).unwrap().is_empty());
        let hits = search_project(&state, id, "кошки", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits.iter().all(|h| h.match_types == ["fts"]));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn limit_is_respected_and_results_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        fs::write(
            paths::projects_dir(&wd).join("other/x.md"),
            "слово из другого проекта",
        )
        .unwrap();
        index::index_project(&state, id).unwrap();
        index::index_project(&state, other.id).unwrap();

        assert_eq!(search_project(&state, id, "слово", 2).unwrap().len(), 2);
        let mut all = rels(&search_project(&state, id, "слово", 100).unwrap());
        all.sort();
        assert_eq!(all, ["a.md", "b.md", "c.md"]);

        let other_hits = search_project(&state, other.id, "слово", 10).unwrap();
        assert_eq!(rels(&other_hits), ["x.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_leg_does_not_leak_across_projects() {
        let (wd, conn, a, _state) = setup("search-sem-scope");
        let b = projects::create_project(&conn, &wd, "other").unwrap();
        fs::write(root(&wd).join("самолет.md"), "самолет летит над морем").unwrap();
        fs::write(
            paths::projects_dir(&wd).join("other/кошки.md"),
            "кошки кошки кошки",
        )
        .unwrap();
        let cfg = crate::config::IndexingConfig::default();
        index::index_root(&conn, a, &root(&wd), &cfg, &FakeEmbedder).unwrap();
        index::index_root(
            &conn,
            b.id,
            &paths::projects_dir(&wd).join("other"),
            &cfg,
            &FakeEmbedder,
        )
        .unwrap();

        // Query semantically closest to B's file: A's hits must carry only A's
        // files, B's rel_path must never surface in A's results.
        let hits_a = search_with(&conn, &FakeEmbedder, 0.3, a, "кошечка", 10).unwrap();
        assert!(
            !hits_a.iter().any(|h| h.rel_path == "кошки.md"),
            "B file leaked into A: {hits_a:?}"
        );
        assert!(hits_a.iter().all(|h| h.rel_path == "самолет.md" || h.rel_path == "datas.md"));

        let hits_b = search_with(&conn, &FakeEmbedder, 0.3, b.id, "кошечка", 10).unwrap();
        assert!(
            hits_b.iter().any(|h| h.rel_path == "кошки.md"),
            "B file missing from B: {hits_b:?}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn query_matching_only_other_project_returns_empty_for_unindexed_project() {
        // FTS-only leg (noop embedder via search_project through AppState).
        let (wd, conn, a, state) = setup("search-scope-fts-empty");
        let b = projects::create_project(&conn, &wd, "other").unwrap();
        fs::write(
            paths::projects_dir(&wd).join("other/кошки.md"),
            "кошки кошки кошки",
        )
        .unwrap();
        index::index_project(&state, b.id).unwrap(); // A stays unindexed

        assert!(search_project(&state, a, "кошки", 10).unwrap().is_empty());
        assert_eq!(rels(&search_project(&state, b.id, "кошки", 10).unwrap()), ["кошки.md"]);

        // Semantic leg with a fake embedder over the same state.
        let fake = FakeEmbedder;
        let cfg = crate::config::IndexingConfig::default();
        index::index_root(&conn, b.id, &paths::projects_dir(&wd).join("other"), &cfg, &fake).unwrap();
        let hits_a = search_with(&conn, &fake, 0.3, a, "кошечка", 10).unwrap();
        assert!(hits_a.is_empty(), "B vectors leaked into A: {hits_a:?}");
        let hits_b = search_with(&conn, &fake, 0.3, b.id, "кошечка", 10).unwrap();
        assert!(
            hits_b.iter().any(|h| h.rel_path == "кошки.md"),
            "B file missing from B: {hits_b:?}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_fts_hit_includes_closed_task_and_carries_task_fields() {
        let (wd, conn, id, state) = setup("search-task-closed");
        let t = tasks::task_create(&conn, id, "Сделать редизайн главной", None).unwrap();
        tasks::task_move(&conn, t.id, "done", 1.0).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.kind, "task");
        assert_eq!(hit.rel_path, "");
        assert_eq!(hit.name, "Сделать редизайн главной");
        assert_eq!(hit.task_id, Some(t.id));
        assert_eq!(hit.status.as_deref(), Some("done"));
        assert_eq!(hit.overdue, Some(false));
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("редизайн"), "{:?}", hit.snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_title_and_comment_are_searchable_tags_are_not() {
        let (wd, conn, id, state) = setup("search-task-fields");
        let t = tasks::task_create(&conn, id, "Задача про баннер", None).unwrap();
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                comment: Some(Some("нужен макет логотипа".into())),
                ..Default::default()
            },
        )
        .unwrap();
        // Tags are out of the model: a legacy tags value on the row (written
        // straight to SQL) must not make the task findable.
        conn.execute("UPDATE tasks SET tags = '[\"полиграфия\"]' WHERE id = ?1", params![t.id])
            .unwrap();

        for query in ["баннер", "логотипа"] {
            let hits = search_project(&state, id, query, 10).unwrap();
            assert_eq!(hits.len(), 1, "query {query:?}");
            assert_eq!(hits[0].kind, "task");
        }
        assert!(search_project(&state, id, "полиграфия", 10).unwrap().is_empty(),
                "tag text must no longer be searchable");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_mutations_keep_search_in_sync() {
        let (wd, conn, id, state) = setup("search-task-sync");
        let find = |q: &str| {
            search_project(&state, id, q, 10)
                .unwrap()
                .iter()
                .any(|h| h.kind == "task")
        };
        let t = tasks::task_create(&conn, id, "старое слово", None).unwrap();
        assert!(find("старое"), "created task must be findable");
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                title: Some("новое слово".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(find("новое"), "updated text must be findable");
        assert!(!find("старое"), "old text must be gone after update");
        tasks::task_delete(&conn, t.id).unwrap();
        assert!(!find("новое"), "deleted task must be gone");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn mixed_query_returns_file_and_task_hits_with_deterministic_ranking() {
        let (wd, conn, id, state) = setup("search-task-mixed");
        fs::write(root(&wd).join("redizayn.md"), "редизайн каталога товаров").unwrap();
        index::index_project(&state, id).unwrap();
        tasks::task_create(&conn, id, "редизайн главной страницы", None).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.kind == "file"));
        assert!(hits.iter().any(|h| h.kind == "task"));
        let again = search_project(&state, id, "редизайн", 10).unwrap();
        let keys = |hs: &[SearchHit]| -> Vec<(String, Option<i64>)> {
            hs.iter().map(|h| (h.kind.clone(), h.task_id)).collect()
        };
        assert_eq!(keys(&hits), keys(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_hits_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-task-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        tasks::task_create(&conn, other.id, "редизайн чужого проекта", None).unwrap();

        assert!(search_project(&state, id, "редизайн", 10).unwrap().is_empty());
        let hits = search_project(&state, other.id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "task");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_leg_adds_task_hit_without_literal_match() {
        let (wd, conn, id, _state) = setup("search-task-sem");
        tasks::task_create(&conn, id, "кошки", None).unwrap();

        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        let task = hits
            .iter()
            .find(|h| h.kind == "task")
            .expect("task semantic hit");
        assert_eq!(task.match_types, ["semantic"]);
        assert!(!task.snippet.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    use crate::chat;
    use crate::harness;
    use crate::llm::Role;

    fn harness_session(conn: &Connection, title: &str) -> (i64, i64) {
        let s = harness::create_session(conn, Some(title)).unwrap();
        (s.id, harness::session_chat_id(conn, s.id).unwrap())
    }

    fn say(conn: &Connection, chat_id: i64, role: Role, text: &str) {
        chat::append_message(conn, chat_id, role, Some(text), &[], None, None).unwrap();
    }

    #[test]
    fn chat_search_finds_harness_session_by_cyrillic_fts() {
        let (wd, conn, _id, _state) = setup("chat-search-fts");
        let (sid, chat_id) = harness_session(&conn, "редизайн витрины");
        say(&conn, chat_id, Role::User, "нужен редизайн главной страницы");
        say(&conn, chat_id, Role::Assistant, "собрал макет, ждём правки");

        let hits = search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.session_id, sid);
        assert_eq!(hit.title.as_deref(), Some("редизайн витрины"));
        assert!(!hit.last_active.is_empty());
        assert!(hit.snippet.contains("<<редизайн>>"), "{:?}", hit.snippet);
        assert_eq!(hit.digest, "нужен редизайн главной страницы");
        assert!(hit.score > 0.0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_digest_prefers_latest_summary_else_first_user_message() {
        let (wd, conn, _id, _state) = setup("chat-search-digest");
        // Summarized chat: the checkpoint with the highest up_to_position wins.
        let (sid_sum, chat_sum) = harness_session(&conn, "summarized");
        say(&conn, chat_sum, Role::User, "реплика пользователя");
        for (pos, text) in [(5, "ранний чекпоинт"), (12, "поздний чекпоинт")] {
            conn.execute(
                "INSERT INTO summaries (chat_id, up_to_position, summary, created_at) VALUES (?1, ?2, ?3, '2026-01-01')",
                params![chat_sum, pos, text],
            )
            .unwrap();
        }
        // Plain chat: the first user message is the digest, not a later one.
        let (sid_first, chat_first) = harness_session(&conn, "plain");
        say(&conn, chat_first, Role::User, "реплика пользователя");
        say(&conn, chat_first, Role::User, "вторая реплика не должна попасть");
        // Long first message: truncated on a char boundary with an ellipsis.
        let (sid_long, chat_long) = harness_session(&conn, "long");
        say(&conn, chat_long, Role::User, &"реплика ".repeat(200));

        let digest_of = |sid: i64| {
            search_chats_with(&conn, &embed::NoopEmbedder, "реплика", 10)
                .unwrap()
                .into_iter()
                .find(|h| h.session_id == sid)
                .expect("hit")
                .digest
        };
        assert_eq!(digest_of(sid_sum), "поздний чекпоинт");
        assert_eq!(digest_of(sid_first), "реплика пользователя");
        let long = digest_of(sid_long);
        assert_eq!(long.chars().count(), CHAT_DIGEST_MAX_CHARS + 1);
        assert!(long.ends_with('…'), "{long}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_groups_by_session_and_ranks_by_sum() {
        let (wd, conn, _id, _state) = setup("chat-search-group");
        let (a, chat_a) = harness_session(&conn, "a");
        let (b, chat_b) = harness_session(&conn, "b");
        say(&conn, chat_a, Role::User, "редизайн каталога");
        say(&conn, chat_a, Role::Assistant, "редизайн корзины готов");
        say(&conn, chat_b, Role::User, "редизайн ленты");

        let hits = search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, a, "two matches must outrank one");
        assert_eq!(hits[1].session_id, b);
        assert!(hits[0].score > hits[1].score);
        // Representative message belongs to session a.
        let a_msgs: Vec<i64> = conn
            .prepare("SELECT m.id FROM messages m JOIN chats c ON c.id = m.chat_id WHERE c.session_id = ?1")
            .unwrap()
            .query_map(params![a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(a_msgs.contains(&hits[0].message_id));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_excludes_project_and_notes_chats() {
        let (wd, conn, pid, _state) = setup("chat-search-scope");
        let project_chat = chat::ensure_project_chat(&conn, pid).unwrap();
        say(&conn, project_chat, Role::User, "редизайн проекта");
        conn.execute(
            "INSERT INTO chats (kind, session_id, created_at, updated_at) VALUES ('notes', NULL, '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let notes_chat = conn.last_insert_rowid();
        say(&conn, notes_chat, Role::User, "редизайн заметок");

        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_semantic_only_hit_without_literal_match() {
        let (wd, conn, _id, _state) = setup("chat-search-sem");
        let (sid, chat_a) = harness_session(&conn, "a");
        let (_, chat_b) = harness_session(&conn, "b");
        say(&conn, chat_a, Role::User, "кошки кошки кошки");
        say(&conn, chat_b, Role::User, "самолет летит над морем");
        for (chat_id, text) in [(chat_a, "кошки кошки кошки"), (chat_b, "самолет летит над морем")] {
            let message_id: i64 = conn
                .query_row(
                    "SELECT id FROM messages WHERE chat_id = ?1 AND role = 'user'",
                    params![chat_id],
                    |r| r.get(0),
                )
                .unwrap();
            let session_id: i64 = conn
                .query_row("SELECT session_id FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
                .unwrap();
            chat::store_message_vector(&conn, &FakeEmbedder, message_id, session_id, text).unwrap();
        }

        let hits = search_chats_with(&conn, &FakeEmbedder, "кошечка", 10).unwrap();
        // KNN has no distance threshold: both sessions surface, the cat one
        // must rank first and carry the fallback snippet.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, sid);
        assert!(hits[0].score > hits[1].score);
        assert!(hits[0].snippet.contains("кошки"), "{:?}", hits[0].snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_survives_injection_and_respects_limit() {
        let (wd, conn, _id, _state) = setup("chat-search-inject");
        for i in 0..3 {
            let (_, chat_id) = harness_session(&conn, "s");
            say(&conn, chat_id, Role::User, &format!("редизайн {i}"));
        }
        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced"] {
            assert!(
                search_chats_with(&conn, &embed::NoopEmbedder, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        assert_eq!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 2).unwrap().len(), 2);
        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "   ", 10).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_drops_index_rows_left_by_deleted_sessions() {
        let (wd, conn, _id, _state) = setup("chat-search-stale");
        let (sid, chat_id) = harness_session(&conn, "a");
        say(&conn, chat_id, Role::User, "редизайн главной");
        assert_eq!(
            search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
                .unwrap()
                .len(),
            1
        );
        // Stale index state: FTS/vector rows survive, the session row does not.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![sid]).unwrap();
        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    use crate::notes;

    #[test]
    fn global_search_finds_all_kinds_tags_hits_and_includes_closed_tasks() {
        let (wd, conn, id, state) = setup("global-kinds");
        fs::write(root(&wd).join("redizayn.md"), "редизайн каталога товаров").unwrap();
        index::index_project(&state, id).unwrap();
        let t = tasks::task_create(&conn, id, "редизайн главной", None).unwrap();
        tasks::task_move(&conn, t.id, "done", 1.0).unwrap();
        notes::ensure(&wd).unwrap();
        fs::write(paths::notes_dir(&wd).join("red.md"), "редизайн заметки").unwrap();
        index::index_notes(&wd).unwrap();
        let (sid, chat_id) = harness_session(&conn, "витрина");
        say(&conn, chat_id, Role::User, "редизайн чата");

        let hits = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        for kind in ["file", "task", "note", "chat"] {
            assert!(kinds.contains(&kind), "missing {kind} in {hits:?}");
        }
        let file = hits.iter().find(|h| h.kind == "file").unwrap();
        assert_eq!(file.project_id, Some(id));
        assert_eq!(file.rel_path.as_deref(), Some("redizayn.md"));
        assert!(file.snippet.contains("<<редизайн>>"), "{:?}", file.snippet);
        let task = hits.iter().find(|h| h.kind == "task").unwrap();
        assert_eq!(task.task_id, Some(t.id));
        assert_eq!(task.project_id, Some(id));
        assert!(task.subtitle.contains("Готово"), "{:?}", task.subtitle);
        let note = hits.iter().find(|h| h.kind == "note").unwrap();
        assert_eq!(note.rel_path.as_deref(), Some("red.md"));
        assert_eq!(note.project_id, None);
        let chat = hits.iter().find(|h| h.kind == "chat").unwrap();
        assert_eq!(chat.session_id, Some(sid));
        // Deterministic merge: the same order on a second run.
        let again = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let keys = |hs: &[GlobalHit]| -> Vec<(String, i64)> {
            hs.iter().map(|h| (h.kind.clone(), h.id)).collect()
        };
        assert_eq!(keys(&hits), keys(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_matches_cyrillic_project_name_case_insensitively() {
        let (wd, conn, _id, _state) = setup("global-project");
        let p = projects::create_project(&conn, &wd, "Ромашка").unwrap();
        let hits = global_search_with(&conn, &embed::NoopEmbedder, "ромашка", 30).unwrap();
        let project = hits.iter().find(|h| h.kind == "project").expect("project hit");
        assert_eq!(project.id, p.id);
        assert_eq!(project.project_id, Some(p.id));
        assert_eq!(project.title, "Ромашка");
        assert!(global_search_with(&conn, &embed::NoopEmbedder, "лютики", 30)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_dedupes_chat_title_and_message_hits_per_session() {
        let (wd, conn, _id, _state) = setup("global-chat-dedupe");
        let (sid, chat_id) = harness_session(&conn, "редизайн витрины");
        say(&conn, chat_id, Role::User, "нужен редизайн главной");
        let hits = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let chats: Vec<&GlobalHit> = hits.iter().filter(|h| h.kind == "chat").collect();
        assert_eq!(chats.len(), 1, "title and message hits must merge: {hits:?}");
        assert_eq!(chats[0].session_id, Some(sid));
        assert_eq!(chats[0].title, "редизайн витрины");
        assert!(chats[0].snippet.contains("<<редизайн>>"), "{:?}", chats[0].snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_sanitizes_input_and_respects_limit() {
        let (wd, conn, id, state) = setup("global-limit");
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        index::index_project(&state, id).unwrap();

        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced"] {
            assert!(
                global_search_with(&conn, &embed::NoopEmbedder, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        assert_eq!(
            global_search_with(&conn, &embed::NoopEmbedder, "слово", 2)
                .unwrap()
                .len(),
            2
        );
        assert!(global_search_with(&conn, &embed::NoopEmbedder, "   ", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
