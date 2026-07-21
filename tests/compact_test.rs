#[test]
fn test_compact_reclaims_space() -> Result<(), Box<dyn std::error::Error>> {
    let path = "/tmp/test_compact.moss";
    let _ = std::fs::remove_file(path);

    let mut moss = moss::storage::Moss::create(path)?;

    for i in 0..100 {
        let data = format!("file_{} content with padding\n", i);
        moss.write_file(&format!("file_{}.txt", i), data.as_bytes())?;
    }
    assert_eq!(moss.entries().count(), 100);

    let before_size = std::fs::metadata(path)?.len();

    for i in 0..50 {
        moss.remove_prefix(&format!("file_{}.txt", i))?;
    }
    assert_eq!(moss.entries().count(), 50);

    moss.sync()?;

    let after_delete_size = std::fs::metadata(path)?.len();
    assert!(after_delete_size > before_size, "append-only should grow");

    moss.compact()?;

    let after_compact_size = std::fs::metadata(path)?.len();
    assert_eq!(moss.entries().count(), 50);
    assert!(
        after_compact_size < after_delete_size,
        "compact should reclaim space: {} < {}",
        after_compact_size,
        after_delete_size
    );

    for i in 50..100 {
        let data = moss.read_file(&format!("file_{}.txt", i))?;
        let expected = format!("file_{} content with padding\n", i);
        assert_eq!(String::from_utf8(data)?, expected);
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

#[test]
fn test_compact_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = "/tmp/test_compact_reopen.moss";
    let _ = std::fs::remove_file(path);

    {
        let mut moss = moss::storage::Moss::create(path)?;
        for i in 0..200 {
            let data = format!("data_chunk_{}\n", i);
            moss.write_file(&format!("chunk_{}.bin", i), data.as_bytes())?;
        }
        moss.remove_prefix("chunk_0.bin")?;
        moss.remove_prefix("chunk_1.bin")?;
        moss.sync()?;
        moss.compact()?;
    }

    let mut moss = moss::storage::Moss::open(path)?;
    assert_eq!(moss.entries().count(), 198);

    for i in 2..200 {
        let data = moss.read_file(&format!("chunk_{}.bin", i))?;
        let expected = format!("data_chunk_{}\n", i);
        assert_eq!(String::from_utf8(data)?, expected);
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}
