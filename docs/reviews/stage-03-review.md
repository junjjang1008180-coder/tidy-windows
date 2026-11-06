# 3단계 독립 검수

## 최초 검수 판정 (구현 당시)

최초 검수에서는 경로 검증 후 일반 경로 문자열로 실행하는 TOCTOU 경쟁(Blocker), 크기/mtime만 비교하는 오래된 계획 검증(Major), 프로세스 중단 뒤 실제 변경 경로와 이력 간 불일치(Major)를 기록했습니다. 해당 기록은 과거 판정이며 아래 후속 보완을 반영합니다.

## 후속 수정 및 재검증

- 원자적 no-replace 이동을 디렉터리 파일 디스크립터 기반으로 바꾸고, Unix 경로 구성요소는 `O_NOFOLLOW`로 열도록 수정했습니다. Linux/macOS 구현은 대상 덮어쓰기를 운영체제 호출 수준에서 막습니다. 코드는 macOS에서 테스트했으며 Linux OS 실행은 미검증입니다.
- 승인 계획에 파일 SHA-256을 저장하고 실행/undo 전에 다시 계산하도록 수정했습니다. 테스트 `stale_plan_and_undo_collision_leave_files_safe` 및 `approved_plan_moves_and_undo_restores_without_overwrite` 통과.
- `cargo test --offline`: 11 tests pass.

## 남은 Major

- 파일시스템 rename과 SQLite 상태 기록은 단일 트랜잭션이 아닙니다. 두 작업 사이 강제 종료 시 앱 시작 때 `interrupted`로 표기하지만 실제 경로 대조/복구 UI는 없습니다. 사용자 데이터 손실로 이어지지는 않더라도 작업 완료 상태 확인이 필요합니다.
- macOS 외 Unix OS, 여러 파일 시스템/볼륨 동작은 검증하지 않았습니다. Windows는 안전한 원자적 no-replace 구현이 없어 파일 작업이 지원되지 않습니다.

**현재 판정: 조건부 통과 (macOS 개발 환경 한정), 릴리스 차단은 7단계 검수에서 재판정.**
