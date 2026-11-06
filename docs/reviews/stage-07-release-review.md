# 7단계 릴리스 후보 독립 검수

## Major — watcher UI 경로 자동 검증 부재

- 위치: `src/ui/App.tsx` 감시 interval/event 경로.
- 30초 timer와 안정화 로직은 구현되었지만 pause/resume, 이벤트 중복, 예외/재시작을 검증하는 UI 통합 테스트가 없습니다.
- 조치: UI 테스트 harness에서 새 파일, 부분 다운로드, 중복 이벤트, 중지/재시작을 검증합니다.

## Major — 중단된 파일 작업의 경로 대조 UI 없음

- 위치: `src-tauri/src/lib.rs` startup interrupted 마킹 및 operation history.
- 강제 종료 시 pending/running은 interrupted로 기록되지만 실제 원본/대상 경로를 대조해 복구하는 UI가 없습니다. 파일시스템 변경과 SQLite 기록은 공동 트랜잭션이 아닙니다.
- 영향: rename은 원자적이지만 재시작 시 이력이 실제 파일 경로와 다를 수 있습니다.

## Major — 플랫폼과 배포 검증 부족

- macOS 개발 호스트만 실행/테스트했습니다. Linux 실행, Windows 빌드, 설치 패키지, 서명 및 업데이트는 미검증입니다. Windows 파일 작업은 안전 구현이 없어 미지원입니다.

## Minor — 릴리스 품질 게이트 공백

- 접근성/키보드, 대규모 폴더/긴 경로/디스크 부족, 설치/DB 복구, 의존성 라이선스/취약점 점검은 미실행입니다.
- 실제 검사: TypeScript check, Vite build, `cargo test --offline` 11 passed. OS/패키징/UI 자동화는 미실행. `cargo fmt --check`는 rustfmt component 미설치로 실행 불가.

## 최종 판정: NO-GO

개발 호스트에서 실행할 수 있지만 사용자 배포 준비가 되지 않았습니다. Major 이슈와 OS별/배포 품질 게이트를 해결해야 합니다.
