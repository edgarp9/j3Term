# 메뉴 기능 검증 기록

검증일: 2026-06-20

## 범위

- 기본 명령 버튼: `cd`, `dir`, `echo hello`, `cls`
- 카테고리 콤보박스와 우클릭 메뉴: New Category, Rename Category, Delete Category, Move Category Up, Move Category Down, Add Button
- 명령 버튼 좌클릭과 우클릭 메뉴: Run Command, Edit Button, Delete Button, Move Button Up, Move Button Down
- 버튼 편집 대화상자, Browse 파일 선택/취소, 토큰 버튼 표시
- 탭 생성/전환/닫기, 마지막 탭 닫기 오류 처리
- 키보드 입력, 버튼 실행 뒤 입력, 오류 뒤 입력
- 분할선 드래그, 창 크기 변경, 설정 저장/복원, 로그 확인
- Linux 빌드/테스트와 Windows 타깃 컴파일 확인

## 제약

- 실제 Windows UI 클릭 자동화는 이 Linux 작업 환경에서 실행하지 못했다. Windows 기준은 현재 Win32 구현 코드와 `cargo check --target x86_64-pc-windows-gnu` 결과를 기준으로 삼았다.
- Linux UI는 `GDK_BACKEND=x11`로 실제 GTK 창을 띄우고 `xdotool`, ImageMagick `import`, `xwininfo`, `xprop`로 클릭/키 입력/대화상자/캡처를 확인했다.
- 추가 설치한 도구는 없다. 실행 중 GTK 설정 경고와 AT-SPI 연결 실패 경고가 로그에 남았지만 앱 로직 오류가 아니라 현재 데스크톱 세션 환경 경고로 분리했다.

## 결과 표

| 메뉴 | 기능 | Windows 동작 | Linux 기존 동작 | 문제 여부 | 원인 | 수정 내용 | 재검증 결과 |
| -- | -- | -- | -- | -- | -- | -- | -- |
| 기본 버튼 | 표시 라벨 | `cd`, `dir`, `echo hello`, `cls` | `pwd`, `ls`, `echo hello`, `clear` | 문제 | Linux 기본 패널이 POSIX 명령 라벨을 직접 노출 | Linux 기본 패널 라벨은 Windows 기준으로 맞추고 실행 명령만 `pwd`, `ls`, `echo`, `clear`로 매핑 | 신규 실행에서 `cd`, `dir`, `echo hello`, `cls` 모두 표시 |
| 기본 버튼 | 실행 명령 echo | 단순 명령은 따옴표 없이 전송 | POSIX 실행 파일명을 항상 quote해 `'pwd'`처럼 보임 | 문제 | POSIX 실행 파일 fragment가 항상 shell literal quote 사용 | 안전한 POSIX 실행 파일명은 quote하지 않고, 공백/메타문자가 있을 때만 quote | `pwd`, `ls`, `echo hello`, `clear`로 전송됨 |
| 기본 버튼 | `cd` | 현재 경로 출력 | `pwd` 기능 등가이나 라벨/echo 차이 존재 | 문제 | Linux 기본 버튼 분기 | 라벨은 `cd`, 실행은 `pwd` | 실제 클릭 시 `/home/edgar` 출력 |
| 기본 버튼 | `dir` | 디렉터리 목록 출력 | `ls` 라벨/명령 | 문제 | Linux 기본 버튼 분기 | 라벨은 `dir`, 실행은 `ls` | 실제 클릭 및 반복 실행 시 목록 출력 |
| 기본 버튼 | `echo hello` | `hello` 출력 | 기능은 같으나 GTK 라벨이 줄임표로 잘릴 수 있음 | 문제 | GTK 버튼 기본 padding과 라벨 폭 제한 | 명령 버튼 CSS padding 축소 | 기본 폭에서 `echo hello` 전체 표시, 클릭 시 `hello` 출력 |
| 기본 버튼 | `cls` | 화면 지움 | `clear` 라벨/명령 | 문제 | Linux 기본 버튼 분기 | 라벨은 `cls`, 실행은 `clear`, `TERM=xterm-256color` 유지 | 실제 클릭 시 화면 정리 |
| 설정 | 저장 위치 | 실행 파일 옆 `j3term.toml` | XDG config 경로에 저장 | 문제 | Linux 전용 설정 경로 분기 | Linux 기본 저장 경로를 실행 파일 옆 `.toml`로 공통화, 기존 XDG 파일은 읽기 fallback 유지 | `/tmp/j3term-ui-target/debug/j3term.toml` 생성/저장/재시작 복원 확인 |
| 키보드 | 직접 입력 | 최상위 창 키 메시지가 터미널 입력으로 전달 | GTK `DrawingArea` 포커스가 잡히지 않으면 `xdotool type` 입력 누락 | 문제 | 키 컨트롤러가 DrawingArea에만 설치됨 | `EventControllerKey`를 `ApplicationWindow`에 설치 | fresh 실행, 버튼 실행 뒤, 마지막 탭 닫기 오류 뒤 모두 입력 정상 |
| 카테고리 메뉴 | 표시/활성 상태 | New/Rename/Delete, Move Up/Down, Add Button | 항목은 대체로 일치 | 없음 | 확인 필요 | 변경 없음 | 마지막 카테고리에서 Delete/Move 비활성, 두 카테고리에서 위치별 Move 활성 확인 |
| 카테고리 메뉴 | New/Rename/Delete/Move | 입력/확인 후 상태 저장 | 동작 가능 | 없음 | 확인 필요 | 변경 없음 | 생성, Esc 취소, Rename, Move Up/Down, Delete No/Yes 모두 확인 |
| 버튼 메뉴 | 표시/활성 상태 | Run/Edit/Delete, Move Up/Down | 항목은 대체로 일치 | 없음 | 확인 필요 | 변경 없음 | 단일 버튼 Move 비활성, 두 버튼 위치별 Move 활성 확인 |
| 버튼 메뉴 | Run/Edit/Delete/Move | 실행/편집/삭제/이동 후 상태 저장 | 동작 가능 | 없음 | 확인 필요 | 변경 없음 | Run, Edit 저장, Delete No/Yes, Move Up/Down 모두 확인 |
| 버튼 편집 | Browse | 파일 선택 대화상자 표시, Cancel 시 편집창 복귀 | 동작 가능 | 없음 | 확인 필요 | 변경 없음 | `Select Executable` 표시, Cancel 후 `Edit Button` 유지 확인 |
| 탭 | 새 탭/전환/닫기 | 탭별 세션 유지, 마지막 탭 닫기 방지 | 동작 가능 | 없음 | 확인 필요 | 변경 없음 | Tab 2 생성, Tab 1 전환, Tab 2 닫기, 마지막 탭 오류 표시 확인 |
| UI 레이아웃 | 버튼 라벨/패널 | 기본 라벨이 버튼 안에 표시 | `echo hello`가 줄임표 처리 | 문제 | GTK 버튼 padding | CSS padding 축소 | 900x520과 760x420에서 기본 라벨 표시 확인 |
| UI 레이아웃 | 카테고리/첫 버튼 간격 | `Default` 드롭다운과 첫 명령 버튼이 분리 표시 | GTK 테마 최소 높이로 두 영역이 겹칠 수 있음 | 문제 | 레이아웃의 카테고리 선택 높이 예약이 GTK 실제 렌더링 높이보다 작음 | 카테고리 선택 예약 높이를 늘리고 첫 버튼과의 간격 회귀 테스트 추가 | 실제 GTK 캡처에서 `Default`와 첫 버튼 분리 표시 확인 |
| 분할선/resize | 드래그와 창 크기 변경 | 즉시 레이아웃/PTY resize | 동작 가능 | 없음 | 확인 필요 | 변경 없음 | 분할선 드래그로 패널 폭 변경, 창 크기 변경 후 입력 정상 |
| 로그/오류 | 사용자 메시지와 내부 원인 분리 | Win32는 app error를 터미널에 표시 | 키 포커스 문제로 오류 뒤 입력 확인이 어려움 | 문제 | GTK 키 컨트롤러 위치 | 창 레벨 키 처리 | 마지막 탭 닫기 후 `[app error: ...]` 표시, 이후 입력 정상 |

## 사용한 검증 명령

- `cargo fmt --check`
- `cargo check`
- `cargo test`
- `cargo check --target x86_64-pc-windows-gnu`
- `CARGO_TARGET_DIR=/tmp/j3term-ui-target cargo build`
- `GDK_BACKEND=x11 /tmp/j3term-ui-target/debug/j3term`
