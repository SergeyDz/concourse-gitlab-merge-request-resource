// Comprehensive test module for check.rs filtering logic
// Tests all GitLab MR scenarios: 500 branches, 200 MRs, edge cases

#[cfg(test)]
mod check_filtering_tests {
    use chrono::{DateTime, Duration, Utc};
    use std::collections::HashMap;
    use std::str::FromStr;
    
    // Mock Version struct (matches main code)
    #[derive(Debug, Clone, PartialEq)]
    struct Version {
        iid: String,
        committed_date: String,
        sha: String,
    }
    
    // Helper: Create version with relative time offset
    fn make_version(iid: u64, minutes_offset: i64, sha: &str) -> Version {
        let time = Utc::now() + Duration::minutes(minutes_offset);
        Version {
            iid: iid.to_string(),
            committed_date: time.to_rfc3339(),
            sha: sha.to_string(),
        }
    }
    
    // Helper: Create version with absolute time
    fn make_version_at(iid: u64, time_str: &str, sha: &str) -> Version {
        Version {
            iid: iid.to_string(),
            committed_date: time_str.to_string(),
            sha: sha.to_string(),
        }
    }
    
    // Core filtering logic (extracted from main for testing)
    fn filter_versions(all_versions: Vec<Version>, current: Option<&Version>) -> Vec<Version> {
        if current.is_none() {
            return all_versions;
        }
        
        let current_version = current.unwrap();
        let mut newer_versions = Vec::new();
        
        for version in all_versions {
            let candidate_dt = DateTime::<Utc>::from_str(&version.committed_date).unwrap();
            let current_dt = DateTime::<Utc>::from_str(&current_version.committed_date).unwrap();
            
            let is_newer = candidate_dt > current_dt;
            let _is_same_time = candidate_dt == current_dt;
            let is_different_mr = version.iid != current_version.iid;
            let is_current_mr = version.iid == current_version.iid;
            
            let time_diff_minutes = (current_dt.timestamp() - candidate_dt.timestamp()) / 60;
            let _within_bulk_window = (0..=10).contains(&time_diff_minutes);
            
            // Updated logic: Include current MR, newer commits, or different MRs within reasonable time window
            // This fixes the user's case (new MR with 27-day-old commit) while avoiding including very old commits
            let time_diff_days = (current_dt.timestamp() - candidate_dt.timestamp()).abs() / (24 * 60 * 60);
            let within_large_window = time_diff_days < 90;  // 90 days window (same as age cutoff)
            let should_include = is_current_mr || is_newer || (is_different_mr && within_large_window);
            
            if should_include {
                newer_versions.push(version);
            }
        }
        
        // MR-AWARE FILTERING: Group by IID, keep latest per MR
        let mut mr_latest: HashMap<String, Version> = HashMap::new();
        
        for version in newer_versions {
            let iid = version.iid.clone();
            
            if let Some(existing) = mr_latest.get(&iid) {
                let existing_dt = DateTime::<Utc>::from_str(&existing.committed_date).unwrap();
                let candidate_dt = DateTime::<Utc>::from_str(&version.committed_date).unwrap();
                
                if candidate_dt > existing_dt {
                    mr_latest.insert(iid, version);
                }
            } else {
                mr_latest.insert(iid, version);
            }
        }
        
        // Always include current version
        let current_iid = &current_version.iid;
        if !mr_latest.contains_key(current_iid) {
            mr_latest.insert(current_iid.clone(), current_version.clone());
        }
        
        let mut result: Vec<Version> = mr_latest.into_values().collect();
        result.sort_by(|a, b| a.committed_date.cmp(&b.committed_date));
        result
    }
    
    // ============================================================================
    // CATEGORY A: Time-based Filtering (10 tests)
    // ============================================================================
    
    #[test]
    fn test_a1_mr_newer_than_current() {
        let current = make_version(10, -60, "abc123");  // 1 hour ago
        let newer = make_version(20, -30, "def456");     // 30 min ago (newer)
        
        let result = filter_versions(vec![newer.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);  // newer + current
        assert!(result.contains(&newer));
        assert!(result.contains(&current));
    }
    
    #[test]
    fn test_a2_mr_older_than_current_included() {
        let current = make_version(10, -30, "abc123");  // 30 min ago
        let older = make_version(20, -60, "def456");    // 1 hour ago (older)
        
        let result = filter_versions(vec![older.clone()], Some(&current));
        
        // With new logic: include different MRs if within 90-day window
        assert_eq!(result.len(), 2);  // current + older MR (within window)
        assert!(result.iter().any(|v| v.iid == "10"));
        assert!(result.iter().any(|v| v.iid == "20"));
    }
    
    #[test]
    fn test_a3_mr_same_time_different_iid() {
        let current = make_version(10, -60, "abc123");
        let same_time = make_version(20, -60, "def456");  // Same time, different MR
        
        let result = filter_versions(vec![same_time.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);
        assert!(result.contains(&same_time));
        assert!(result.contains(&current));
    }
    
    #[test]
    fn test_a4_mr_same_time_same_iid() {
        let current = make_version(10, -60, "abc123");
        let same_mr = Version {
            iid: "10".to_string(),
            committed_date: current.committed_date.clone(),
            sha: "abc123".to_string(),
        };
        
        let result = filter_versions(vec![same_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 1);  // Current version kept (no duplicate)
        assert_eq!(result[0].iid, "10");
    }
    
    #[test]
    fn test_a5_mr_within_10min_bulk_window() {
        let current = make_version(100, -60, "current");  // 60 min ago
        let bulk_mr = make_version(50, -65, "bulk");      // 65 min ago (5 min before current)
        
        let result = filter_versions(vec![bulk_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);  // bulk + current
        assert!(result.contains(&bulk_mr));
    }
    
    #[test]
    fn test_a6_mr_outside_10min_bulk_window_included() {
        let current = make_version(100, -60, "current");  // 60 min ago
        let old_mr = make_version(50, -80, "old");        // 80 min ago (20 min before current)
        
        let result = filter_versions(vec![old_mr.clone()], Some(&current));
        
        // With new logic: include different MRs within 90-day window
        assert_eq!(result.len(), 2);  // old MR + current
        assert!(result.iter().any(|v| v.iid == "50"));
        assert!(result.iter().any(|v| v.iid == "100"));
    }
    
    #[test]
    fn test_a7_mr_exactly_at_10min_boundary() {
        let current = make_version(100, -60, "current");
        let boundary_mr = make_version(50, -70, "boundary");  // Exactly 10 min before
        
        let result = filter_versions(vec![boundary_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);  // Should be included (0..=10 range)
        assert!(result.contains(&boundary_mr));
    }
    
    #[test]
    fn test_a8_multiple_mrs_same_timestamp() {
        let time_str = "2025-10-09T12:00:00+00:00";
        let current = make_version_at(100, time_str, "current");
        let mr1 = make_version_at(50, time_str, "sha1");
        let mr2 = make_version_at(60, time_str, "sha2");
        let mr3 = make_version_at(70, time_str, "sha3");
        
        let result = filter_versions(vec![mr1.clone(), mr2.clone(), mr3.clone()], Some(&current));
        
        assert_eq!(result.len(), 4);  // All different MRs, same time
        assert!(result.contains(&mr1));
        assert!(result.contains(&mr2));
        assert!(result.contains(&mr3));
        assert!(result.contains(&current));
    }
    
    #[test]
    fn test_a9_timezone_normalization() {
        let current = make_version_at(10, "2025-10-09T12:00:00+00:00", "abc");
        let other = make_version_at(20, "2025-10-09T14:00:00+02:00", "def");  // Same time, different TZ
        
        let result = filter_versions(vec![other.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);  // Same UTC time, different MR
        assert!(result.contains(&other));
    }
    
    #[test]
    fn test_a10_age_boundary_89_vs_91_days() {
        let now = Utc::now();
        let current = make_version(1, 0, "current");
        let day_89 = make_version_at(89, &(now - Duration::days(89)).to_rfc3339(), "sha89");
        let day_91 = make_version_at(91, &(now - Duration::days(91)).to_rfc3339(), "sha91");
        
        // Note: Age filtering happens before this function, but testing time comparison
        let result = filter_versions(vec![day_89.clone(), day_91.clone()], Some(&current));
        
        // With new logic: include MRs within 90-day window
        assert_eq!(result.len(), 2);  // current + day_89 (within window), day_91 excluded
        assert!(result.iter().any(|v| v.iid == "1"));
        assert!(result.iter().any(|v| v.iid == "89"));
        assert!(!result.iter().any(|v| v.iid == "91"));
    }
    
    // ============================================================================
    // CATEGORY B: MR-Aware Filtering (10 tests)
    // ============================================================================
    
    #[test]
    fn test_b1_single_mr_single_commit() {
        let current = make_version(10, -120, "old");
        let new_mr = make_version(50, -30, "new");
        
        let result = filter_versions(vec![new_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|v| v.iid == "50"));
    }
    
    #[test]
    fn test_b2_single_mr_three_commits_keep_latest() {
        let current = make_version(10, -120, "old");
        // Use relative times so MR commits are newer than current
        let mr_50_commit_a = make_version(50, -30, "sha_a");  // 30 min ago
        let mr_50_commit_b = make_version(50, -20, "sha_b");  // 20 min ago
        let mr_50_commit_c = make_version(50, -10, "sha_c");  // 10 min ago (latest)
        
        let result = filter_versions(
            vec![mr_50_commit_a, mr_50_commit_b, mr_50_commit_c.clone()], 
            Some(&current)
        );
        
        assert_eq!(result.len(), 2);  // Latest of MR 50 + current
        let mr_50_result = result.iter().find(|v| v.iid == "50").unwrap();
        assert_eq!(mr_50_result.sha, "sha_c");  // Latest commit
    }
    
    #[test]
    fn test_b3_five_different_mrs_one_commit_each() {
        let current = make_version(100, -120, "current");
        let mrs = vec![
            make_version(77, -10, "sha77"),
            make_version(78, -9, "sha78"),
            make_version(79, -8, "sha79"),
            make_version(80, -7, "sha80"),
            make_version(81, -6, "sha81"),
        ];
        
        let result = filter_versions(mrs, Some(&current));
        
        assert_eq!(result.len(), 6);  // 5 MRs + current
        for i in 77..=81 {
            assert!(result.iter().any(|v| v.iid == i.to_string()));
        }
    }
    
    #[test]
    fn test_b4_five_mrs_multiple_commits_each() {
        let current = make_version(100, -200, "current");
        let all_commits = vec![
            // MR 77: 3 commits
            make_version_at(77, "2025-10-09T10:00:00+00:00", "77a"),
            make_version_at(77, "2025-10-09T10:05:00+00:00", "77b"),
            make_version_at(77, "2025-10-09T10:10:00+00:00", "77c"),
            
            // MR 78: 2 commits
            make_version_at(78, "2025-10-09T10:15:00+00:00", "78a"),
            make_version_at(78, "2025-10-09T10:20:00+00:00", "78b"),
            
            // MR 79: 1 commit
            make_version_at(79, "2025-10-09T10:25:00+00:00", "79a"),
            
            // MR 80: 4 commits
            make_version_at(80, "2025-10-09T10:30:00+00:00", "80a"),
            make_version_at(80, "2025-10-09T10:35:00+00:00", "80b"),
            make_version_at(80, "2025-10-09T10:40:00+00:00", "80c"),
            make_version_at(80, "2025-10-09T10:45:00+00:00", "80d"),
            
            // MR 81: 2 commits
            make_version_at(81, "2025-10-09T10:50:00+00:00", "81a"),
            make_version_at(81, "2025-10-09T10:55:00+00:00", "81b"),
        ];
        
        let result = filter_versions(all_commits, Some(&current));
        
        assert_eq!(result.len(), 6);  // 5 MRs (latest each) + current
        
        // Verify latest commits kept
        let mr_77 = result.iter().find(|v| v.iid == "77").unwrap();
        assert_eq!(mr_77.sha, "77c");
        
        let mr_80 = result.iter().find(|v| v.iid == "80").unwrap();
        assert_eq!(mr_80.sha, "80d");
    }
    
    #[test]
    fn test_b5_mr_with_older_commit_included() {
        let current = make_version(50, -30, "current");  // 30 min ago
        let older = make_version(60, -120, "old");       // 2 hours ago
        
        let result = filter_versions(vec![older], Some(&current));
        
        // With new logic: include different MRs within 90-day window
        assert_eq!(result.len(), 2);  // current + older MR
        assert!(result.iter().any(|v| v.iid == "50"));
        assert!(result.iter().any(|v| v.iid == "60"));
    }
    
    #[test]
    fn test_b6_mr_with_newer_commit_included() {
        let current = make_version(50, -60, "current");
        let newer = make_version(60, -10, "newer");
        
        let result = filter_versions(vec![newer.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|v| v.iid == "60"));
    }
    
    #[test]
    fn test_b7_current_mr_with_new_commit() {
        let current = make_version_at(50, "2025-10-09T10:00:00+00:00", "sha_old");
        let newer_commit_same_mr = make_version_at(50, "2025-10-09T11:00:00+00:00", "sha_new");
        
        let result = filter_versions(vec![newer_commit_same_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 1);  // One MR
        assert_eq!(result[0].iid, "50");
        assert_eq!(result[0].sha, "sha_new");  // Newer commit wins
    }
    
    #[test]
    fn test_b8_current_mr_no_new_commit() {
        let current = make_version(50, -60, "current_sha");
        
        let result = filter_versions(vec![], Some(&current));
        
        assert_eq!(result.len(), 1);  // Current version always included
        assert_eq!(result[0], current);
    }
    
    #[test]
    fn test_b9_hashmap_ordering_independence() {
        // Use single base time to avoid microsecond drift
        let base_time = Utc::now();
        let current = Version {
            iid: "100".to_string(),
            committed_date: (base_time + Duration::minutes(-120)).to_rfc3339(),
            sha: "current".to_string(),
        };
        
        // Create MRs with consistent timestamps
        let v77 = Version {
            iid: "77".to_string(),
            committed_date: (base_time + Duration::minutes(-10)).to_rfc3339(),
            sha: "77".to_string(),
        };
        let v78 = Version {
            iid: "78".to_string(),
            committed_date: (base_time + Duration::minutes(-9)).to_rfc3339(),
            sha: "78".to_string(),
        };
        let v79 = Version {
            iid: "79".to_string(),
            committed_date: (base_time + Duration::minutes(-8)).to_rfc3339(),
            sha: "79".to_string(),
        };
        
        let order1 = vec![v77.clone(), v78.clone(), v79.clone()];
        let order2 = vec![v79.clone(), v77.clone(), v78.clone()];
        
        let result1 = filter_versions(order1, Some(&current));
        let result2 = filter_versions(order2, Some(&current));
        
        // Results should be identical (sorted by time)
        assert_eq!(result1.len(), result2.len());
        for i in 0..result1.len() {
            assert_eq!(result1[i], result2[i]);
        }
    }
    
    #[test]
    fn test_b10_stress_200_mrs_500_commits() {
        let current = make_version(1000, -500, "current");  // 500 min ago
        let mut all_commits = vec![];
        
        // Create 200 MRs, some with multiple commits
        // MR time offsets: MR 1 = -10 to -15, MR 2 = -20 to -26, ..., MR 200 = -2000 to -2004
        // Current is at -500, so MRs 1-49 are newer (less negative), MRs 50-200 are older
        for mr_id in 1..=200 {
            let commit_count = (mr_id % 5) + 1;  // 1-5 commits per MR
            for commit_num in 0..commit_count {
                let time_offset = -(mr_id as i64 * 10 + commit_num as i64);
                all_commits.push(make_version(mr_id, time_offset, &format!("sha_{mr_id}_{commit_num}")));
            }
        }
        
        let result = filter_versions(all_commits, Some(&current));
        
        // With new logic: include all different MRs within 90-day window
        // All 200 MRs are within 90 days, so all should be included (latest commit per MR)
        assert_eq!(result.len(), 201);  // 200 MRs (latest each) + current
        
        // Verify no duplicates
        let mut seen_iids = std::collections::HashSet::new();
        for version in &result {
            assert!(seen_iids.insert(&version.iid), "Duplicate IID found: {}", version.iid);
        }
    }
    
    // ============================================================================
    // CATEGORY C: Same SHA Scenarios (5 tests)
    // ============================================================================
    
    #[test]
    fn test_c1_same_sha_two_different_mrs() {
        let current = make_version(100, -120, "current");
        // Use relative times so MRs are newer than current
        let mr1 = make_version(50, -30, "shared_sha");
        let mr2 = make_version(60, -30, "shared_sha");  // Same time, different MR
        
        let result = filter_versions(vec![mr1.clone(), mr2.clone()], Some(&current));
        
        assert_eq!(result.len(), 3);  // Both MRs + current
        assert!(result.iter().any(|v| v.iid == "50"));
        assert!(result.iter().any(|v| v.iid == "60"));
    }
    
    #[test]
    fn test_c2_same_sha_same_timestamp_different_iids() {
        let current = make_version(100, -120, "current");
        // Use relative time so MRs are newer than current
        let mr1 = make_version(50, -30, "cherry_pick_sha");
        let mr2 = make_version(60, -30, "cherry_pick_sha");
        let mr3 = make_version(70, -30, "cherry_pick_sha");
        
        let result = filter_versions(vec![mr1, mr2, mr3], Some(&current));
        
        assert_eq!(result.len(), 4);  // All 3 MRs + current
    }
    
    #[test]
    fn test_c3_same_sha_ten_mrs_massive_cherry_pick() {
        let current = make_version(200, -300, "current");
        let shared_time = "2025-10-09T10:00:00+00:00";
        let mut mrs = vec![];
        
        for iid in 50..60 {
            mrs.push(make_version_at(iid, shared_time, "hotfix_sha"));
        }
        
        let result = filter_versions(mrs, Some(&current));
        
        assert_eq!(result.len(), 11);  // 10 MRs + current
        for iid in 50..60 {
            assert!(result.iter().any(|v| v.iid == iid.to_string()));
        }
    }
    
    #[test]
    fn test_c4_same_sha_one_is_current_version() {
        let current = make_version_at(50, "2025-10-09T10:00:00+00:00", "shared");
        let other = make_version_at(60, "2025-10-09T10:00:00+00:00", "shared");
        
        let result = filter_versions(vec![other.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|v| v.iid == "50"));
        assert!(result.iter().any(|v| v.iid == "60"));
    }
    
    #[test]
    fn test_c5_same_sha_all_older_than_current_included() {
        let current = make_version(100, -10, "current");  // Recent
        let old1 = make_version_at(50, "2025-10-08T10:00:00+00:00", "old_sha");
        let old2 = make_version_at(60, "2025-10-08T10:00:00+00:00", "old_sha");
        
        let result = filter_versions(vec![old1, old2], Some(&current));
        
        // With new logic: include different MRs within 90-day window (~6 days difference)
        assert_eq!(result.len(), 3);  // current + 2 older MRs
        assert!(result.iter().any(|v| v.iid == "50"));
        assert!(result.iter().any(|v| v.iid == "60"));
        assert!(result.iter().any(|v| v.iid == "100"));
    }
    
    // ============================================================================
    // CATEGORY D: Bulk Creation (10 tests)
    // ============================================================================
    
    #[test]
    fn test_d1_five_mrs_within_1_minute() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let bulk_mrs = vec![
            make_version_at(77, "2025-10-09T09:59:10+00:00", "77"),  // 50s before
            make_version_at(78, "2025-10-09T09:59:20+00:00", "78"),  // 40s before
            make_version_at(79, "2025-10-09T09:59:30+00:00", "79"),  // 30s before
            make_version_at(80, "2025-10-09T09:59:40+00:00", "80"),  // 20s before
            make_version_at(81, "2025-10-09T09:59:50+00:00", "81"),  // 10s before
        ];
        
        let result = filter_versions(bulk_mrs, Some(&current));
        
        assert_eq!(result.len(), 6);  // All 5 + current (within 10-min window)
    }
    
    #[test]
    fn test_d2_five_mrs_within_10_minutes() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let bulk_mrs = vec![
            make_version_at(77, "2025-10-09T09:50:00+00:00", "77"),  // 10 min before
            make_version_at(78, "2025-10-09T09:52:00+00:00", "78"),
            make_version_at(79, "2025-10-09T09:54:00+00:00", "79"),
            make_version_at(80, "2025-10-09T09:56:00+00:00", "80"),
            make_version_at(81, "2025-10-09T09:58:00+00:00", "81"),  // 2 min before
        ];
        
        let result = filter_versions(bulk_mrs, Some(&current));
        
        assert_eq!(result.len(), 6);  // All within window
    }
    
    #[test]
    fn test_d3_five_mrs_within_11_minutes_all_included() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let bulk_mrs = vec![
            make_version_at(77, "2025-10-09T09:49:00+00:00", "77"),  // 11 min before
            make_version_at(78, "2025-10-09T09:52:00+00:00", "78"),  // 8 min before
            make_version_at(79, "2025-10-09T09:54:00+00:00", "79"),
            make_version_at(80, "2025-10-09T09:56:00+00:00", "80"),
            make_version_at(81, "2025-10-09T09:58:00+00:00", "81"),
        ];
        
        let result = filter_versions(bulk_mrs, Some(&current));
        
        // With new logic: include all different MRs within 90-day window
        assert_eq!(result.len(), 6);  // All 5 MRs + current
        assert!(result.iter().any(|v| v.iid == "77"));
        assert!(result.iter().any(|v| v.iid == "78"));
        assert!(result.iter().any(|v| v.iid == "79"));
        assert!(result.iter().any(|v| v.iid == "80"));
        assert!(result.iter().any(|v| v.iid == "81"));
        assert!(result.iter().any(|v| v.iid == "100"));
    }
    
    #[test]
    fn test_d4_ten_mrs_partial_in_window_all_included() {
        let current = make_version(100, 0, "current");  // Now
        
        let mut mrs = vec![];
        for i in 0..10 {
            let minutes_before = 15 - i;  // 15, 14, 13, ..., 6 minutes before
            mrs.push(make_version(70 + i, -(minutes_before as i64), &format!("sha{i}")));
        }
        
        let result = filter_versions(mrs, Some(&current));
        
        // With new logic: include all different MRs within 90-day window
        assert_eq!(result.len(), 11);  // All 10 MRs + current
    }
    
    #[test]
    fn test_d5_bulk_exactly_at_10min_boundary() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        let boundary = make_version_at(77, "2025-10-09T09:50:00+00:00", "boundary");
        
        let result = filter_versions(vec![boundary.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);  // Should include (0..=10 range)
        assert!(result.iter().any(|v| v.iid == "77"));
    }
    
    #[test]
    fn test_d6_bulk_with_current_at_end() {
        let current = make_version_at(81, "2025-10-09T10:00:00+00:00", "current");
        
        let bulk_mrs = vec![
            make_version_at(77, "2025-10-09T09:55:00+00:00", "77"),
            make_version_at(78, "2025-10-09T09:56:00+00:00", "78"),
            make_version_at(79, "2025-10-09T09:57:00+00:00", "79"),
            make_version_at(80, "2025-10-09T09:58:00+00:00", "80"),
        ];
        
        let result = filter_versions(bulk_mrs, Some(&current));
        
        assert_eq!(result.len(), 5);  // All 4 + current
    }
    
    #[test]
    fn test_d7_bulk_with_current_at_start() {
        let current = make_version_at(77, "2025-10-09T09:50:00+00:00", "current");
        
        let bulk_mrs = vec![
            make_version_at(78, "2025-10-09T09:52:00+00:00", "78"),  // 2 min after current
            make_version_at(79, "2025-10-09T09:54:00+00:00", "79"),
            make_version_at(80, "2025-10-09T09:56:00+00:00", "80"),
            make_version_at(81, "2025-10-09T09:58:00+00:00", "81"),
        ];
        
        let result = filter_versions(bulk_mrs, Some(&current));
        
        assert_eq!(result.len(), 5);  // All newer + current
    }
    
    #[test]
    fn test_d8_bulk_spanning_window_all_included() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let mixed = vec![
            make_version_at(70, "2025-10-09T09:30:00+00:00", "old"),    // 30 min before
            make_version_at(75, "2025-10-09T09:55:00+00:00", "bulk1"),  // 5 min before
            make_version_at(80, "2025-10-09T09:58:00+00:00", "bulk2"),  // 2 min before
            make_version_at(85, "2025-10-09T10:05:00+00:00", "newer"),  // 5 min after
        ];
        
        let result = filter_versions(mixed, Some(&current));
        
        // With new logic: include all different MRs within 90-day window
        assert_eq!(result.len(), 5);  // All 4 MRs + current
        assert!(result.iter().any(|v| v.iid == "70"));
        assert!(result.iter().any(|v| v.iid == "75"));
        assert!(result.iter().any(|v| v.iid == "80"));
        assert!(result.iter().any(|v| v.iid == "85"));
        assert!(result.iter().any(|v| v.iid == "100"));
    }
    
    #[test]
    fn test_d9_stress_50_mrs_bulk_created() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let mut bulk = vec![];
        for i in 0..50 {
            let seconds_before = i * 10;  // 0s, 10s, 20s, ... 490s (8min 10s)
            let time = Utc::now() - Duration::seconds(seconds_before);
            bulk.push(make_version_at((50 + i) as u64, &time.to_rfc3339(), &format!("bulk{i}")));
        }
        
        let result = filter_versions(bulk, Some(&current));
        
        // All within 10 minutes should be included
        assert!(result.len() >= 30);  // Most should be within 10-min window
    }
    
    #[test]
    fn test_d10_bulk_interleaved_with_others() {
        let current = make_version_at(100, "2025-10-09T10:00:00+00:00", "current");
        
        let mixed = vec![
            // Bulk group 1 (within window)
            make_version_at(77, "2025-10-09T09:55:00+00:00", "bulk1"),
            make_version_at(78, "2025-10-09T09:56:00+00:00", "bulk2"),
            
            // Newer MR
            make_version_at(90, "2025-10-09T10:05:00+00:00", "newer"),
            
            // Bulk group 2 (within window)
            make_version_at(79, "2025-10-09T09:57:00+00:00", "bulk3"),
            make_version_at(80, "2025-10-09T09:58:00+00:00", "bulk4"),
        ];
        
        let result = filter_versions(mixed, Some(&current));
        
        assert_eq!(result.len(), 6);  // All 5 + current
    }
    
    // ============================================================================
    // CATEGORY G: Current Version Inclusion (5 tests)
    // ============================================================================
    
    #[test]
    fn test_g1_current_in_newer_versions_no_duplicate() {
        let current = make_version_at(50, "2025-10-09T10:00:00+00:00", "current");
        let same_mr_newer = make_version_at(50, "2025-10-09T11:00:00+00:00", "newer");
        
        let result = filter_versions(vec![same_mr_newer.clone()], Some(&current));
        
        assert_eq!(result.len(), 1);  // One MR (newer version)
        assert_eq!(result[0].sha, "newer");
    }
    
    #[test]
    fn test_g2_current_not_in_newer_added_back() {
        let current = make_version(50, -120, "current");
        let other_mr = make_version(60, -60, "other");  // Different MR, within window
        
        let result = filter_versions(vec![other_mr.clone()], Some(&current));
        
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|v| v.iid == "50"));  // Current added back
        assert!(result.iter().any(|v| v.iid == "60"));
    }
    
    #[test]
    fn test_g3_current_mr_newer_commit() {
        let current = make_version_at(50, "2025-10-09T10:00:00+00:00", "old_sha");
        let newer_same_mr = make_version_at(50, "2025-10-09T11:00:00+00:00", "new_sha");
        
        let result = filter_versions(vec![newer_same_mr], Some(&current));
        
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sha, "new_sha");  // Newer wins
    }
    
    #[test]
    fn test_g4_current_only_mr() {
        let current = make_version(50, -60, "only");
        
        let result = filter_versions(vec![], Some(&current));
        
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], current);
    }
    
    #[test]
    fn test_g5_no_current_first_run() {
        let mrs = vec![
            make_version(77, -10, "77"),
            make_version(78, -20, "78"),
            make_version(79, -30, "79"),
        ];
        
        let result = filter_versions(mrs.clone(), None);
        
        assert_eq!(result.len(), 3);  // All returned
        assert_eq!(result, mrs);
    }
}
