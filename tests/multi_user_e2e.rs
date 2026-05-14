use microclaw::db::Database;

fn test_db() -> (Database, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("microclaw_mte2e_{}", uuid::Uuid::new_v4()));
    let db = Database::new(dir.to_str().unwrap()).unwrap();
    (db, dir)
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn multi_user_chat_memory_usage_isolation() {
    let (db, dir) = test_db();

    let alice = "user-alice";
    let bob = "user-bob";

    let alice_chat = db
        .resolve_or_create_chat_id(alice, "web", "alice-main", Some("alice-main"), "web")
        .unwrap();
    let bob_chat = db
        .resolve_or_create_chat_id(bob, "web", "bob-main", Some("bob-main"), "web")
        .unwrap();
    assert_ne!(alice_chat, bob_chat, "distinct chat ids per user");

    assert_eq!(db.get_chat_user_id(alice_chat).unwrap().as_deref(), Some(alice));
    assert_eq!(db.get_chat_user_id(bob_chat).unwrap().as_deref(), Some(bob));

    db.insert_memory_with_metadata(
        alice,
        Some(alice_chat),
        "alice favourite colour: blue",
        "PREFERENCE",
        "explicit",
        0.95,
    )
    .unwrap();
    db.insert_memory_with_metadata(
        alice,
        None,
        "alice global memory: dietary vegan",
        "PREFERENCE",
        "explicit",
        0.9,
    )
    .unwrap();
    db.insert_memory_with_metadata(
        bob,
        Some(bob_chat),
        "bob favourite colour: red",
        "PREFERENCE",
        "explicit",
        0.95,
    )
    .unwrap();
    db.insert_memory_with_metadata(
        bob,
        None,
        "bob global memory: dietary omnivore",
        "PREFERENCE",
        "explicit",
        0.9,
    )
    .unwrap();

    let alice_view = db.get_memories_for_context(alice, alice_chat, 100).unwrap();
    let bob_view = db.get_memories_for_context(bob, bob_chat, 100).unwrap();

    assert_eq!(alice_view.len(), 2, "alice sees only her own memories");
    for m in &alice_view {
        assert!(m.content.starts_with("alice "), "{}", m.content);
    }
    assert_eq!(bob_view.len(), 2, "bob sees only his own memories");
    for m in &bob_view {
        assert!(m.content.starts_with("bob "), "{}", m.content);
    }

    let alice_cross = db.get_memories_for_context(alice, bob_chat, 100).unwrap();
    assert_eq!(
        alice_cross.len(),
        1,
        "alice scoped to bob's chat sees only alice global memories"
    );
    assert!(alice_cross[0].content.starts_with("alice "));

    db.log_llm_usage(alice, alice_chat, "web", "anthropic", "claude-test", 100, 50, "agent_loop")
        .unwrap();
    db.log_llm_usage(alice, alice_chat, "web", "anthropic", "claude-test", 200, 80, "agent_loop")
        .unwrap();
    db.log_llm_usage(bob, bob_chat, "web", "anthropic", "claude-test", 1000, 500, "agent_loop")
        .unwrap();

    let alice_usage = db.get_llm_usage_summary(Some(alice_chat)).unwrap();
    let bob_usage = db.get_llm_usage_summary(Some(bob_chat)).unwrap();
    assert_eq!(alice_usage.input_tokens, 300);
    assert_eq!(alice_usage.output_tokens, 130);
    assert_eq!(bob_usage.input_tokens, 1000);
    assert_eq!(bob_usage.output_tokens, 500);

    let chats = db.get_recent_chats(None, 100).unwrap();
    assert!(chats.iter().any(|c| c.chat_id == alice_chat));
    assert!(chats.iter().any(|c| c.chat_id == bob_chat));

    cleanup(&dir);
}

#[test]
fn distinct_users_can_share_session_key_label() {
    let (db, dir) = test_db();

    let alice_chat = db
        .resolve_or_create_chat_id("user-alice", "web", "main", Some("main"), "web")
        .unwrap();
    let bob_chat = db
        .resolve_or_create_chat_id("user-bob", "web", "main", Some("main"), "web")
        .unwrap();
    assert_ne!(
        alice_chat, bob_chat,
        "two users with the same session_key label get separate chats"
    );

    let again_alice = db
        .resolve_or_create_chat_id("user-alice", "web", "main", Some("main"), "web")
        .unwrap();
    assert_eq!(again_alice, alice_chat, "alice's main is stable on re-resolve");

    cleanup(&dir);
}

#[test]
fn delete_user_data_purges_chats_memories_usage_for_target_user_only() {
    let (db, dir) = test_db();

    let alice = "user-alice";
    let bob = "user-bob";
    let alice_chat = db
        .resolve_or_create_chat_id(alice, "web", "alice-main", Some("alice-main"), "web")
        .unwrap();
    let bob_chat = db
        .resolve_or_create_chat_id(bob, "web", "bob-main", Some("bob-main"), "web")
        .unwrap();

    db.insert_memory_with_metadata(alice, Some(alice_chat), "alice fact", "KNOWLEDGE", "explicit", 0.9)
        .unwrap();
    db.insert_memory_with_metadata(bob, Some(bob_chat), "bob fact", "KNOWLEDGE", "explicit", 0.9)
        .unwrap();
    db.log_llm_usage(alice, alice_chat, "web", "anthropic", "claude-test", 100, 50, "agent_loop")
        .unwrap();
    db.log_llm_usage(bob, bob_chat, "web", "anthropic", "claude-test", 200, 80, "agent_loop")
        .unwrap();

    let rows = db.delete_user_data(alice).unwrap();
    assert!(rows > 0);

    assert_eq!(db.get_chat_user_id(alice_chat).unwrap(), None);
    assert_eq!(db.get_chat_user_id(bob_chat).unwrap().as_deref(), Some(bob));

    let alice_mems = db.get_memories_for_context(alice, alice_chat, 100).unwrap();
    let bob_mems = db.get_memories_for_context(bob, bob_chat, 100).unwrap();
    assert!(alice_mems.is_empty(), "alice memories should be purged");
    assert_eq!(bob_mems.len(), 1, "bob memories untouched");

    let alice_usage = db.get_llm_usage_summary(Some(alice_chat)).unwrap();
    let bob_usage = db.get_llm_usage_summary(Some(bob_chat)).unwrap();
    assert_eq!(alice_usage.input_tokens, 0);
    assert_eq!(bob_usage.input_tokens, 200);

    cleanup(&dir);
}

#[test]
fn cross_user_memory_lookup_isolates_globals() {
    let (db, dir) = test_db();

    let alice = "user-alice";
    let bob = "user-bob";
    db.resolve_or_create_chat_id(alice, "web", "main", Some("main"), "web")
        .unwrap();
    db.insert_memory_with_metadata(alice, None, "alice global fact", "PREFERENCE", "explicit", 0.9)
        .unwrap();
    db.insert_memory_with_metadata(bob, None, "bob global fact", "PREFERENCE", "explicit", 0.9)
        .unwrap();

    let alice_view = db.get_memories_for_context(alice, 0, 100).unwrap();
    let bob_view = db.get_memories_for_context(bob, 0, 100).unwrap();
    assert_eq!(alice_view.len(), 1);
    assert!(alice_view[0].content.starts_with("alice "));
    assert_eq!(bob_view.len(), 1);
    assert!(bob_view[0].content.starts_with("bob "));

    cleanup(&dir);
}

#[test]
fn archive_excess_memories_scopes_to_user() {
    let (db, dir) = test_db();

    let alice = "user-alice";
    let bob = "user-bob";
    let alice_chat = db
        .resolve_or_create_chat_id(alice, "web", "main", Some("main"), "web")
        .unwrap();
    let bob_chat = db
        .resolve_or_create_chat_id(bob, "web", "main", Some("main"), "web")
        .unwrap();
    for i in 0..6 {
        db.insert_memory_with_metadata(
            alice,
            Some(alice_chat),
            &format!("alice fact {i}"),
            "KNOWLEDGE",
            "explicit",
            0.5,
        )
        .unwrap();
    }
    for i in 0..6 {
        db.insert_memory_with_metadata(
            bob,
            Some(bob_chat),
            &format!("bob fact {i}"),
            "KNOWLEDGE",
            "explicit",
            0.5,
        )
        .unwrap();
    }

    let archived = db.archive_excess_memories(alice, Some(alice_chat), 3).unwrap();
    assert_eq!(archived, 3, "should archive alice's 3 lowest-confidence");
    let bob_active = db.get_memories_for_context(bob, bob_chat, 100).unwrap();
    assert_eq!(bob_active.len(), 6, "bob's memories untouched by alice archive");

    cleanup(&dir);
}

#[test]
fn list_session_meta_with_user_filter_excludes_other_users() {
    let (db, dir) = test_db();
    let alice = "user-alice";
    let bob = "user-bob";
    let alice_chat = db
        .resolve_or_create_chat_id(alice, "web", "alice-tree", Some("alice-tree"), "web")
        .unwrap();
    let bob_chat = db
        .resolve_or_create_chat_id(bob, "web", "bob-tree", Some("bob-tree"), "web")
        .unwrap();
    db.save_session(alice_chat, r#"[{"role":"user","content":"alice"}]"#)
        .unwrap();
    db.save_session(bob_chat, r#"[{"role":"user","content":"bob"}]"#)
        .unwrap();

    let alice_rows = db.list_session_meta(Some(alice), 100).unwrap();
    let alice_ids: Vec<i64> = alice_rows.iter().map(|r| r.0).collect();
    assert!(alice_ids.contains(&alice_chat));
    assert!(
        !alice_ids.contains(&bob_chat),
        "alice must not see bob's session metadata"
    );

    let bob_rows = db.list_session_meta(Some(bob), 100).unwrap();
    let bob_ids: Vec<i64> = bob_rows.iter().map(|r| r.0).collect();
    assert!(bob_ids.contains(&bob_chat));
    assert!(
        !bob_ids.contains(&alice_chat),
        "bob must not see alice's session metadata"
    );

    let op_rows = db.list_session_meta(None, 100).unwrap();
    let op_ids: Vec<i64> = op_rows.iter().map(|r| r.0).collect();
    assert!(op_ids.contains(&alice_chat));
    assert!(op_ids.contains(&bob_chat));

    cleanup(&dir);
}

#[tokio::test]
async fn memory_backend_routes_user_id_through_provider() {
    use std::sync::Arc;

    let (db, dir) = test_db();
    let backend = microclaw::memory_backend::MemoryBackend::local_only(Arc::new(db));

    let alice = "user-alice";
    let bob = "user-bob";

    backend
        .insert_memory_with_metadata(alice, None, "alice secret", "KNOWLEDGE", "explicit", 0.9)
        .await
        .unwrap();
    backend
        .insert_memory_with_metadata(bob, None, "bob secret", "KNOWLEDGE", "explicit", 0.9)
        .await
        .unwrap();

    let alice_mem = backend.get_memories_for_context(alice, 0, 100).await.unwrap();
    let bob_mem = backend.get_memories_for_context(bob, 0, 100).await.unwrap();

    assert!(
        alice_mem.iter().any(|m| m.content == "alice secret"),
        "alice should see her global memory"
    );
    assert!(
        !alice_mem.iter().any(|m| m.content == "bob secret"),
        "alice must not see bob's global memory through provider"
    );
    assert!(
        bob_mem.iter().any(|m| m.content == "bob secret"),
        "bob should see his global memory"
    );
    assert!(
        !bob_mem.iter().any(|m| m.content == "alice secret"),
        "bob must not see alice's global memory through provider"
    );

    cleanup(&dir);
}

#[test]
fn cross_user_chat_owner_lookup_returns_correct_user() {
    let (db, dir) = test_db();

    let chats: Vec<(String, i64)> = vec![
        ("user-a".to_string(), 0),
        ("user-b".to_string(), 0),
        ("user-c".to_string(), 0),
    ]
    .into_iter()
    .map(|(u, _)| {
        let cid = db
            .resolve_or_create_chat_id(&u, "web", "chat", Some("chat"), "web")
            .unwrap();
        (u, cid)
    })
    .collect();

    for (uid, cid) in &chats {
        let owner = db.get_chat_user_id(*cid).unwrap();
        assert_eq!(owner.as_deref(), Some(uid.as_str()));
    }

    cleanup(&dir);
}
