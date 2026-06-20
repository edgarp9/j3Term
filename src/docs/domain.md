# j3Term 도메인 노트

## 요구사항

- Windows와 Linux를 같은 코드베이스에서 지원하는 데스크톱 터미널 앱이다.
- Windows UI는 Win32 창과 메시지 루프를 직접 사용하고, Linux UI는 GTK4 애플리케이션과 위젯 이벤트를 사용한다.
- 실제 셸 프로세스는 `portable_pty` 기반 PTY로 실행한다. Windows에서는 ConPTY, Linux에서는 OS PTY를 사용한다.
- 터미널 파싱과 화면 상태는 `alacritty_terminal`의 `Term`, `Parser`, grid 모델을 사용한다.
- 렌더링은 초기 구조에서 GDI 텍스트 출력으로 제한하고, GPU/고급 스타일링은 이후 확장 대상으로 둔다.
- 기본 창은 왼쪽 터미널 렌더링 영역과 오른쪽 명령 패널로 나뉜다.
- 터미널 렌더링 영역은 내부에 8px padding을 두며, PTY 행/열 계산은 이 padding을 제외한 터미널 콘텐츠 영역 기준으로 한다.
- 왼쪽 터미널 렌더링 영역은 PTY 출력 scrollback을 보관하고, 마우스 휠과 세로 스크롤바로 이전/이후 출력을 스크롤해 볼 수 있어야 한다.
- 사용자는 왼쪽 터미널 콘텐츠 영역에서 마우스로 드래그해 현재 보이는 터미널 셀 범위를 선택할 수 있어야 한다.
- 선택된 터미널 텍스트가 있으면 `Ctrl+C`는 선택 텍스트를 Windows 클립보드에 복사하고 PTY에는 `Ctrl+C`를 전달하지 않는다. 선택 텍스트가 없으면 기존처럼 `Ctrl+C`를 PTY 입력으로 전달한다.
- 사용자는 `Ctrl+V`로 플랫폼 클립보드의 Unicode 텍스트를 활성 터미널 세션에 붙여넣을 수 있어야 한다. 붙여넣기 텍스트의 줄바꿈은 사용자가 Enter를 누른 것과 같은 터미널 입력으로 정규화한다.
- 오른쪽 명령 패널 상단에는 카테고리 선택 드롭다운을 두고, 아래에는 선택된 카테고리에 속한 명령 버튼 목록을 표시한다.
- 카테고리 선택 드롭다운과 첫 명령 버튼은 플랫폼 네이티브 컨트롤의 최소 렌더링 높이를 고려해 서로 겹치지 않는 여백을 유지해야 한다.
- 명령 버튼 목록은 버튼 수가 하단 영역의 세로 공간을 넘으면 스크롤바로 시작 위치를 이동해 숨겨진 버튼에 접근할 수 있어야 한다.
- 왼쪽 터미널 렌더링 영역과 오른쪽 명령 패널 사이에는 마우스로 드래그 가능한 분할선을 둔다.
- 사용자가 분할선을 드래그하면 터미널 영역과 명령 패널의 폭을 즉시 재계산하고, 모든 탭의 PTY와 터미널 화면 크기를 새 터미널 영역에 맞춘다.
- 기본 창 클라이언트 요청 크기는 750x520 px이다.
- 기본 창 상단에는 압축된 크기의 탭 바를 두고, 각 탭은 독립적인 터미널 세션을 가진다.
- 앱은 시작 시 기본 탭 하나를 만들고, 사용자는 탭 바의 새 탭 버튼으로 새 셸 세션을 열 수 있다.
- 앱은 시작 시 실제 클라이언트 크기로 초기 레이아웃과 터미널 크기를 맞춘 뒤, 활성 터미널에 시작 시점의 클라이언트 크기를 안내한다.
- 사용자는 탭을 클릭해 활성 탭을 전환하고, 탭 닫기 버튼으로 현재 또는 비활성 탭의 세션을 종료할 수 있다.
- 마지막 탭은 앱이 열린 동안 유지한다. 마지막 탭 닫기는 복구 가능한 앱 오류로 처리한다.
- 명령 버튼과 키보드 입력은 항상 활성 탭의 터미널 세션으로 전달한다.
- 사용자는 명령 패널의 카테고리 드롭다운을 우클릭해 카테고리 생성, 이름 변경, 삭제, 위/아래 이동, 현재 카테고리에 버튼 추가를 실행할 수 있다.
- 카테고리 생성 시 사용자는 새 카테고리 이름을 입력할 수 있고, 기존 카테고리 이름도 같은 이름 입력 흐름으로 변경할 수 있다.
- 사용자는 명령 버튼을 우클릭해 실행, 편집, 삭제, 위/아래 이동을 실행할 수 있다.
- 사용자는 카테고리 또는 명령 버튼 우클릭 메뉴에서 시스템에 설치된 고정폭 폰트와 폰트 크기를 선택해 터미널 표시 폰트를 변경할 수 있다.
- 사용자는 카테고리 또는 명령 버튼 우클릭 메뉴에서 About 창을 열어 프로그램 버전과 `https://github.com/edgarp9` 링크를 확인할 수 있어야 한다.
- 버튼 편집창은 버튼 이름, 실행 파일/명령, 실행 인자를 편집한다. 실행 인자에는 `{path}`, `{name}`, `{selectfile}`, `{selectdir}`, `{inputtext}` 토큰을 삽입할 수 있다.
- `{path}`와 `{name}`은 버튼 실행 시 현재 터미널 셸의 현재 경로와 마지막 경로 이름으로 해석되도록 셸별 표현으로 변환한다. `{selectfile}`, `{selectdir}`, `{inputtext}`는 실행 시 플랫폼 파일 선택, 폴더 선택, 텍스트 입력 창을 열어 값을 받은 뒤 인자에 반영한다.
- 실행 시 입력 토큰이 여러 번 등장하면 한 번 받은 값을 같은 실행 안에서 재사용한다. 사용자가 파일/폴더/텍스트 입력을 취소하면 버튼 실행도 취소한다.
- 명령 패널 설정과 터미널 폰트 설정은 앱 시작 시 로드하고, 카테고리/버튼 편집, 선택 카테고리 변경, 폰트 변경 후 저장한다.
- 설정 파일은 프로그램 실행 파일과 같은 폴더에 실행 파일 이름의 확장자를 `.toml`로 바꾼 경로로 저장한다.
- 카테고리 삭제는 마지막 카테고리를 남겨야 하며, 삭제되는 카테고리 안의 버튼도 함께 제거한다.
- Win32 메시지 루프는 `RegisterClassW`, `CreateWindowExW`, `GetMessageW`, `DispatchMessageW` 흐름을 기준으로 구성한다.
- GTK4 실행 흐름은 `gtk4::Application` 활성화, `ApplicationWindow` 구성, `glib` timeout/source 이벤트를 기준으로 구성한다.
- 앱 아이콘은 `icon.ico`를 Windows 실행파일 리소스에 포함하고, 같은 리소스를 프로그램 창의 big/small 아이콘으로 사용한다.
- Linux GTK application id와 desktop entry/icon id는 `io.github.edgarp9.j3Term`이다.
- Linux desktop entry와 아이콘은 앱 실행 시 자동 등록하지 않고, 사용자가 `--install` 또는 `--uninstall` CLI 명령을 명시했을 때만 사용자 영역에 설치/제거한다.
- 릴리즈 빌드는 프로젝트 루트의 `build_release.py`로 실행하며, 스크립트는 Windows와 Linux에서 같은 명령 형태로 동작해야 한다.
- Windows 릴리즈 실행 파일은 GUI 서브시스템으로 링크해 앱 실행 시 별도 콘솔 창을 띄우지 않는다. 디버그 빌드는 개발 중 오류 출력을 확인할 수 있도록 기존 콘솔 동작을 유지한다.
- 실행 파일에 전달된 첫 번째 커맨드라인 인자가 기존 폴더 경로이면 Windows Notepad가 파일 경로 인자를 받아 바로 여는 흐름처럼, 첫 PTY 셸의 시작 작업 디렉터리로 적용한다.
- 폴더 경로 뒤에 남은 인자나 폴더가 아닌 인자는 앱 시작 후 현재 PTY 세션에 한 번 전달해 실행한다.
- 복구 가능한 오류는 앱 경계에서 `Result`로 전달하고 사용자에게 표시할 메시지와 내부 원인을 분리한다.
- interactive 명령은 PTY stdin/stdout 흐름을 그대로 사용한다. 앱은 버튼 명령 실행 여부만으로 입력 전달을 막지 않는다.
- password prompt처럼 echo가 꺼진 입력은 앱이 별도 상태, 로그, 오류 메시지에 원문을 저장하지 않는다.

## 용어

- `TerminalSession`: 앱 계층의 유스케이스 객체. 현재 터미널 크기와 세션 상태를 보관하고 `start`, `write_input`, `resize`, `shutdown` 흐름을 `PtyBackend`에 위임한다.
- `TerminalTabs`: 앱 계층의 탭 유스케이스 객체. 여러 `TerminalSession`을 소유하고 활성 탭, 새 탭 생성, 탭 전환, 탭 종료, 전체 resize/shutdown 흐름을 조정한다.
- `TerminalTab`: 하나의 탭에 속한 셸 프로세스, PTY, 터미널 화면 상태, 표시 제목을 묶은 실행 단위.
- `TerminalTabId`: 탭을 식별하는 도메인 값. Win32 컨트롤 ID나 벡터 인덱스를 상위 계층에 노출하지 않기 위한 타입이다.
- `TerminalTabView`: 렌더링 계층에 전달하는 탭 표시 모델. 탭 식별자, 제목, 활성 여부만 포함한다.
- `PtyBackend`: infra 계층의 PTY 포트. 실제 셸 프로세스, PTY reader/writer, 크기 변경, 종료 정리를 담당하며 앱 계층에는 `TerminalEvent`와 `Result`만 노출한다.
- `TerminalEvent`: PTY에서 앱으로 올라오는 이벤트. 출력 바이트, stdin write 실패, PTY 종료, child process 종료 코드, 사용자 표시 메시지와 내부 원인이 분리된 실패 정보를 표현한다.
- `session_status_after_event`: `TerminalEvent`가 들어왔을 때 현재 `SessionStatus`가 다음 상태로 어떻게 바뀌는지 결정하는 순수 도메인 규칙이다. 출력 이벤트는 현재 상태를 유지하고, PTY/child 종료는 `Exited`, stdin/backend 실패는 `Failed`로 전이한다.
- `InteractiveSession`: 사용자가 실행 중인 셸 또는 하위 프로그램과 PTY를 통해 상호작용하는 세션 규칙. `ssh` password 입력, `set /p`, `Read-Host`, 기타 stdin 대기 프로그램은 모두 같은 PTY writer 경로로 입력을 받는다.
- `TerminalViewport`: 현재 viewport의 터미널 화면 스냅샷. `alacritty_terminal` grid에서 복사한 행/열 크기, `TerminalCell` 목록, `CursorPosition`, 스크롤바 동기화를 위한 `TerminalScrollState`를 포함하며 app/domain에서 조회 가능한 형태만 노출한다.
- `TerminalGridPoint`: 현재 보이는 `TerminalViewport` 안의 0-based 행/열 좌표. 마우스 픽셀 좌표를 터미널 셀 좌표로 변환한 뒤 선택 규칙에 사용한다.
- `TerminalSelection`: 사용자가 드래그로 만든 터미널 셀 선택 범위. anchor와 focus 두 좌표를 보관하고, 복사/렌더링 시 행별 선형 선택 범위로 정규화한다.
- `TerminalPaste`: 클립보드 문자열을 PTY stdin으로 보낼 byte sequence로 변환하는 입력 규칙. `\r\n`과 `\n`은 `\r`로 정규화해 셸에서 Enter 입력처럼 처리되게 한다.
- `TerminalScroll`: 사용자가 터미널 scrollback 표시 위치를 이동하려는 요청. 마우스 휠의 줄 단위 이동, 스크롤바의 페이지/절대 위치/끝점 이동을 표현한다.
- `TerminalScrollState`: 현재 터미널 scrollback 표시 위치, 최대 위치, 페이지 크기, 전체 행 수를 담는 스크롤바 동기화 모델이다.
- `TerminalCell`: 터미널 grid의 한 셀. 현재는 표시 문자만 보관하며 색상, 스타일, wide character 보조 정보는 이후 확장 대상으로 둔다.
- `CursorPosition`: 현재 커서의 0-based 행/열 위치. 외부 크레이트의 `Point`, `Line`, `Column` 타입을 상위 계층에 노출하지 않기 위한 도메인 타입이다.
- `TerminalViewportPort`: 앱 계층이 터미널 상태 모델에 기대하는 포트. PTY 출력 바이트 반영, 터미널 응답 바이트 회수, resize, `TerminalViewport` 스냅샷 생성을 분리한다.
- 터미널 세션: 하나의 셸 프로세스, PTY, 터미널 화면 상태를 묶은 실행 단위.
- 터미널 크기: PTY와 grid가 공유하는 행, 열, 픽셀 크기.
- `TerminalInput`: 사용자가 터미널에 전달하려는 입력을 표현하는 도메인 타입. 문자 입력, C0/DEL 제어 바이트, 방향키 같은 터미널 키 입력을 포함하며 최종적으로 PTY에 쓸 byte sequence로 변환된다.
- `TerminalKey`: `TerminalInput` 중 문자로 표현되지 않는 키. 현재는 방향키를 지원하며 필요하면 Home/End/Delete/Fn 키로 확장한다.
- `TerminalKeyModifiers`: Shift, Alt, Ctrl 상태. 방향키 modifier는 xterm CSI modifier parameter로 변환한다.
- 입력 이벤트: Win32 키/문자 메시지에서 `TerminalInput`으로 변환된 값.
- `CommandPanel`: 창 오른쪽 명령 패널의 도메인 모델. 카테고리 목록, 현재 선택된 카테고리, 다음 카테고리/버튼 ID 발급 상태를 가진다.
- `CommandCategory`: 명령 버튼을 묶는 1단계 분류. 식별자, 표시 이름, 버튼 목록을 가진다.
- `CommandCategoryId`: 카테고리를 식별하는 도메인 값. Win32 콤보박스 인덱스나 메뉴 ID를 상위 계층에 노출하지 않기 위한 타입이다.
- `CommandButton`: 창 오른쪽 명령 패널에 표시되는 실행 버튼. 버튼은 식별자, 표시 라벨, 실행 파일/명령, `CommandArguments`를 가진다.
- `CommandButtonId`: 명령 버튼을 식별하는 도메인 값. Win32 컨트롤 ID와 독립적으로 유지한다.
- `CommandButtonDefinition`: 버튼 편집창과 도메인 모델 사이에서 전달되는 편집 가능한 버튼 정의. 표시 라벨, 실행 파일/명령, 실행 인자를 포함한다.
- `CommandArguments`: 버튼 실행 파일/명령에 전달할 인자 문자열. 실행 전 `{path}`, `{name}`, `{selectfile}`, `{selectdir}`, `{inputtext}` 토큰을 해석할 수 있다.
- `ButtonArgumentInputs`: `CommandArguments`에 들어 있는 실행 시 입력 토큰 요구사항. 플랫폼 UI 어댑터가 어떤 입력창을 띄울지 결정하는 데 사용한다.
- `ButtonArgumentValues`: 실행 시 사용자가 선택하거나 입력한 파일, 폴더, 텍스트 값이다. 하나의 버튼 실행 안에서는 같은 토큰에 같은 값을 재사용한다.
- `CommandText`: 버튼으로 실행할 셸 명령 문자열. 앱 계층에서 PTY stdin으로 보낼 때 Enter 입력에 해당하는 `\r`을 붙여 즉시 실행 가능한 byte sequence로 변환한다.
- `ShellCommandDialect`: 현재 PTY 기본 셸이 명령 프롬프트, PowerShell, POSIX 셸 중 무엇인지 나타내는 값. `{path}`와 `{name}` 같은 셸 현재 경로 토큰을 각 셸 문법으로 변환할 때 사용한다.
- `ButtonCommand`: 사용자가 명령 버튼을 클릭해 현재 PTY 세션 stdin으로 미리 정의된 `CommandText`를 전송하는 유스케이스.
- `StartupDirectory`: 프로세스 시작 인자에서 얻은 최초 작업 디렉터리. 첫 번째 OS 인자가 실제 폴더이면 `entry`가 이를 시작 옵션으로 전달하고, PTY 어댑터가 기본 셸 `CommandBuilder`의 working directory로 적용한다.
- `StartupCommand`: 프로세스 시작 인자에서 만든 최초 실행 명령. 인자가 없으면 생성하지 않고, 인자가 있으면 각 인자를 셸에서 실행 가능한 한 줄 명령으로 조합한 뒤 `\r`을 붙여 현재 PTY stdin으로 한 번 전송한다.
- `StartupInvocation`: 앱 시작 시 한 번 적용할 `StartupDirectory`와 `StartupCommand`를 묶은 도메인 값. `entry`가 구성하고 플랫폼 UI/app/PTY 경계는 이 값을 전달하거나 필요한 하위 값만 참조한다.
- `CommandPanelSettings`: 명령 패널의 카테고리와 버튼 정의를 TOML 파일로 저장하기 위한 설정 모델. 런타임 식별자는 저장하지 않고 로드 시 도메인이 새로 부여한다.
- `TerminalFont`: 터미널 렌더링에 사용할 폰트 family와 point 크기를 담는 설정 값이다. 현재 렌더링 격자를 유지하기 위해 플랫폼 폰트 선택 UI는 고정폭 폰트를 우선 선택하게 한다. GTK/Pango가 일부 CJK TTC 폰트의 고정폭 여부를 노출하지 못할 수 있으므로, GTK 폰트 선택 UI는 family 이름의 `Mono`/`Monospace` 표기도 터미널 폰트 후보로 인정한다.
- `AppSettings`: 명령 패널 설정과 `TerminalFont`를 하나의 TOML 파일로 읽고 쓰기 위한 앱 설정 모델이다.
- `SettingsPath`: 앱 설정 TOML을 읽고 쓰는 경로다. `std::env::current_exe()`로 얻은 실행 파일 경로의 확장자를 `.toml`로 바꾼 경로를 사용한다.
- 명령 패널: 카테고리 드롭다운과 선택된 카테고리의 버튼 컨트롤이 배치되는 창 오른쪽 영역.
- 명령 버튼 목록 영역: 명령 패널에서 카테고리 드롭다운 아래에 있는 스크롤 가능한 버튼 배치 영역. 카테고리 드롭다운의 네이티브 최소 높이와 헤더 간격을 제외한 영역에서 시작한다. 스크롤 위치는 도메인 레이아웃 계산의 입력이며, 실제 플랫폼 스크롤바 컨트롤은 infra 계층이 동기화한다.
- 터미널 렌더링 영역: GDI 텍스트 렌더링과 키보드 입력 포커스의 기준이 되는 창 왼쪽 영역.
- 터미널 콘텐츠 영역: 터미널 렌더링 영역에서 내부 padding을 제외한 실제 텍스트, 커서, PTY 행/열 계산 영역.
- 터미널 scrollback: 왼쪽 터미널 렌더링 영역에 보이지 않는 과거 출력 행. 활성 탭의 터미널 버퍼가 소유하고, 스크롤 위치는 탭별로 독립적이다.
- 영역 분할선: 터미널 렌더링 영역과 명령 버튼 영역 사이의 드래그 가능한 세로 영역. 사용자가 누른 위치의 오프셋을 유지하며 이동하고, 너무 좁은 창에서도 양쪽 영역이 사라지지 않도록 폭을 clamp한다.
- 창 레이아웃: 클라이언트 크기에서 오른쪽 명령 버튼 영역, 영역 분할선, 버튼 배치, 왼쪽 터미널 렌더링 영역을 계산한 값.
- 시작 창 크기 안내: 플랫폼 창 생성 직후 실제 클라이언트 크기를 기준으로 활성 터미널에 표시하는 앱 상태 메시지다.
- 탭 바: 클라이언트 상단의 탭 표시/클릭 영역. 탭 배치, 닫기 버튼, 새 탭 버튼의 hit target을 계산한다.
- `ApplicationIcon`: 플랫폼 창에 적용되는 앱 아이콘. Windows 빌드 시 `icon.ico`를 PE 리소스에 포함하고, Linux GTK4 창은 `io.github.edgarp9.j3Term` 아이콘 이름과 실행 파일 폴더 또는 현재 작업 폴더의 `icon.svg`/`icon.png` 런타임 아이콘 파일을 사용한다.
- `ApplicationIdentity`: 사용자에게 표시하는 앱 이름, Cargo 패키지 버전, 작성자 GitHub 프로필 링크를 묶은 제품 정보다. About 창은 이 정보를 표시한다.
- `ReleaseBuildScript`: 릴리즈 바이너리 생성을 담당하는 프로젝트 루트의 Python 스크립트. Cargo release 빌드를 실행하고 성공 후 빌드 산출물 폴더를 OS 파일 관리자에서 연다.

## 유스케이스

- 앱 시작: 플랫폼 창을 만들고 앱 아이콘을 적용한 뒤 기본 터미널 크기로 셸 세션을 시작한다. 실제 클라이언트 크기로 resize한 뒤 활성 터미널에 시작 창 크기 안내를 표시하고, 시작 인자 명령이 있으면 그 다음에 실행한다.
- 설정 로드: 앱 시작 전 `infra::config`가 `SettingsPath`를 계산하고 설정 파일을 읽어 `CommandPanel`과 `TerminalFont`를 만든다. 파일이 없으면 기본 명령 패널과 기본 폰트 설정을 사용한다.
- 설정 저장: 카테고리 생성/이름 변경/삭제/이동, 버튼 추가/편집/삭제/이동, 선택 카테고리 변경, 폰트 변경이 성공하면 현재 `AppSettings`를 `SettingsPath`의 TOML 파일로 저장한다.
- 폰트 변경: 플랫폼 UI가 시스템 폰트 선택 대화상자를 열고 사용자가 고정폭 폰트와 크기를 고르면 `TerminalFont`를 갱신한다. 새 폰트의 셀 메트릭으로 터미널 행/열을 다시 계산하고 모든 탭의 PTY 크기를 맞춘다.
- About 표시: 플랫폼 UI가 현재 실행 중인 프로그램 버전과 작성자 GitHub 프로필 링크를 포함한 모달 About 창을 연다. 사용자가 작성자 GitHub 프로필 링크를 클릭하면 기본 브라우저로 해당 URL을 연다. 이 흐름은 앱 상태나 설정 파일을 변경하지 않는다.
- 기본 탭 시작: 앱 계층은 시작 시 `TerminalTabs`에 기본 탭 하나를 만들고 활성 탭으로 지정한다. startup command가 있으면 창과 PTY resize가 끝난 뒤 활성 탭에 한 번만 전송한다.
- 새 탭 생성: 사용자가 새 탭 버튼을 클릭하면 앱 계층은 현재 터미널 크기로 새 `TerminalSession`을 만들고 셸을 시작한 뒤 활성 탭으로 전환한다. 최대 탭 수에 도달하면 복구 가능한 앱 오류를 활성 탭에 표시한다.
- 탭 전환: 사용자가 탭을 클릭하면 앱 계층은 활성 `TerminalTabId`만 변경한다. 각 탭의 PTY와 터미널 viewport는 독립적으로 유지한다.
- 탭 닫기: 사용자가 탭 닫기 버튼을 클릭하면 해당 탭의 `TerminalSession::shutdown`을 실행한 뒤 탭 목록에서 제거한다. PTY 정리가 비동기 pending 상태로 남아도 탭 UI는 즉시 제거하고, 닫은 탭이 활성 탭이면 왼쪽 이웃 탭을 우선 활성화한다. 마지막 탭은 닫지 않는다.
- 탭 이벤트 처리: `WM_TIMER`에서 활성 탭의 PTY 이벤트를 매번 drain하고, 비활성 탭은 하나씩 순환 drain해 viewport를 최신 상태로 따라잡게 한다. 화면 invalidate는 활성 탭에 표시 변화가 있을 때 수행한다.
- 릴리즈 빌드: 개발자가 `python build_release.py` 또는 `python3 build_release.py`를 실행하면 스크립트가 프로젝트 루트 기준으로 현재 호스트의 Cargo release 빌드를 수행한다. 필요한 경우 `--target`으로 Windows 또는 Linux 타깃을 명시한다. 빌드 성공 후 산출물 폴더를 파일 관리자에서 연다.
- 시작 인자 실행: `entry`가 실행 파일 이름 뒤의 OS 인자를 수집해 `StartupInvocation`을 만든다. 첫 번째 인자가 기존 폴더이면 `StartupDirectory`로 적용하고, 나머지 인자는 `StartupCommand`로 변환한다. 플랫폼 창과 PTY 세션이 정상 시작되고 실제 클라이언트 크기에 맞춰 resize된 뒤, 앱 계층은 `StartupCommand`가 있을 때만 PTY stdin으로 전송한다. 인자가 없거나 폴더 경로만 있으면 기존처럼 기본 셸만 표시한다.
- Linux desktop 통합 설치: `entry`가 `--install` 단독 인자를 받으면 GTK 앱을 시작하지 않고 `infra::linux_desktop`에 사용자 영역 desktop entry와 아이콘 설치를 위임한다. desktop entry는 현재 실행 파일 절대 경로를 `Exec`에 쓰며, 같은 내용이면 다시 쓰지 않는다. `icon.svg`가 있으면 SVG를 우선 설치하고 stale PNG를 제거하며, SVG가 없고 `icon.png`가 있으면 PNG fallback을 설치한다.
- Linux desktop 통합 제거: `entry`가 `--uninstall` 단독 인자를 받으면 설치된 main/alias desktop entry와 아이콘을 제거하고, 예전 app id나 실행 파일 이름 기반 legacy 파일도 함께 정리한다. 대상 파일이 이미 없어도 성공해야 한다.
- 세션 시작: `TerminalSession::start`가 현재 크기로 `PtyBackend`를 시작한다. Windows 기본 셸은 infra에서 `cmd.exe` 또는 PowerShell 중 하나로 선택하고, Linux 기본 셸은 `$SHELL` 또는 `/bin/sh`를 선택한다.
- 셸 출력 수신: PTY reader thread가 `portable_pty` reader에서 읽은 byte stream을 `TerminalEvent::PtyOutput`으로 `std::sync::mpsc` 채널에 전달한다. reader EOF는 `TerminalEvent::PtyClosed`로 구분하고, 일시적인 interrupted read는 실패 이벤트로 만들지 않고 다시 읽는다. child process 종료는 별도의 `TerminalEvent::ChildExited`로 전달한다. UI thread는 `WM_TIMER`에서 이벤트를 drain하고, 앱 계층의 `TerminalSession`이 `TerminalViewportPort`를 통해 `alacritty_terminal` parser/state model에 반영한다.
- 터미널 이벤트 상태 전이: `TerminalSession::drain_events`는 각 `TerminalEvent`를 처리할 때 도메인 규칙인 `session_status_after_event`로 세션 상태를 먼저 갱신한 뒤, 사용자에게 보여줄 짧은 상태 메시지를 터미널 viewport에 반영한다. 실패 원인의 상세 문자열은 UI 경계의 `last_error`에만 보관한다.
- 터미널 응답 전달: `alacritty_terminal`이 커서 위치 보고 같은 `PtyWrite` 이벤트를 만들면 `TerminalViewportPort`가 응답 바이트로 모아 둔다. `TerminalSession`은 PTY 출력 반영 직후 이 바이트를 `PtyBackend::write_input`으로 돌려보내 셸이 터미널 질의에 응답받도록 한다.
- 화면 스냅샷 조회: 렌더링은 터미널 상태 모델을 직접 수정하지 않고, `TerminalSession::terminal_viewport`가 생성한 `TerminalViewport` 스냅샷을 읽어 그린다. 이 스냅샷은 Win32 API 타입을 포함하지 않는다.
- 사용자 입력 전달: 플랫폼 입력 이벤트를 `TerminalInput`으로 변환하고 `TerminalSession::write_input`을 통해 PTY writer에 바이트로 전송한다. 앱은 입력 문자를 화면에 직접 추가하지 않고, 셸이나 실행 중인 프로그램이 PTY 출력으로 echo한 내용만 터미널 화면 상태에 반영한다.
- 터미널 출력 스크롤: 플랫폼 마우스 휠 또는 터미널 스크롤바 입력이 발생하면 UI 어댑터는 입력을 `TerminalScroll`로 변환하고, 앱 계층은 활성 탭의 터미널 버퍼 표시 오프셋을 갱신한 뒤 `TerminalViewport`를 다시 조회해 화면과 스크롤바 위치를 갱신한다.
- 터미널 텍스트 선택: 사용자가 터미널 콘텐츠 셀 위에서 왼쪽 버튼을 누르고 드래그하면 플랫폼 어댑터가 마우스 위치를 `TerminalGridPoint`로 변환한다. 선택 상태는 뷰 어댑터가 현재 활성 viewport 기준으로 보관하며, 렌더러는 `TerminalSelection`의 행별 범위에 선택 배경을 그린다. 단일 클릭은 기존 선택을 지우고 새 선택을 만들지 않는다.
- 터미널 선택 복사: 선택 텍스트가 있는 상태에서 `Ctrl+C`가 들어오면 뷰 어댑터가 현재 `TerminalViewport`와 `TerminalSelection`으로 복사 문자열을 만들고, 플랫폼 클립보드에 Unicode 텍스트로 저장한다. 이 경우 같은 키 입력에서 생성될 문자 이벤트는 무시해 PTY에 중복 전달하지 않는다.
- 터미널 붙여넣기: `Ctrl+V`가 들어오면 플랫폼 클립보드 어댑터가 Unicode 텍스트를 읽고, 앱 계층이 붙여넣기 문자열을 터미널 입력 byte sequence로 변환해 활성 탭의 PTY writer로 전달한다. 붙여넣기 원문은 별도 로그나 상태에 저장하지 않는다.
- stdin 요구 프로그램 입력: `ssh` password prompt처럼 실행 중인 프로그램이 stdin을 요구하는 동안에도 동일한 PTY writer 경로를 사용한다. 입력의 echo 여부는 PTY 내부 프로그램의 터미널 모드가 결정한다.
- InteractiveSession 입력 보장: 버튼 명령은 PTY stdin에 명령 텍스트와 Enter를 쓰는 동작일 뿐 세션을 busy 상태로 만들지 않는다. 버튼 클릭 후에도 포커스와 키보드 메시지는 메인 터미널 입력 경로로 되돌아와야 하며, 사용자가 치는 모든 문자/제어키는 현재 PTY writer로 전달된다.
- InteractiveSession 보안: 앱은 사용자가 입력한 byte sequence를 별도 버퍼에 누적하거나 로그로 남기지 않는다. stdin write 실패 이벤트에도 실패 원인만 담고 입력 원문은 포함하지 않는다. 화면에 보이는 내용은 셸 또는 실행 중인 프로그램이 PTY stdout/stderr로 echo한 출력에 한정한다.
- 창 크기 변경: `WM_SIZE`에서 클라이언트 영역으로 `WindowLayout`을 다시 계산하고, GDI 고정폭 폰트의 `CellMetrics`로 터미널 콘텐츠 영역의 행/열과 픽셀 크기를 산출한다. `TerminalTabs::resize`는 모든 탭의 `TerminalSession::resize`를 호출해 각 PTY와 상태 모델 크기를 맞춘다.
- 좌우 영역 크기 조절: 사용자가 터미널과 명령 버튼 영역 사이의 분할선을 누르면 Win32 마우스 캡처를 시작한다. 드래그 중 `WM_MOUSEMOVE`에서 분할선 위치를 클라이언트 폭 안에서 clamp하고, `TerminalTabs::resize`를 호출해 활성/비활성 탭의 PTY와 viewport 크기를 함께 갱신한다. `WM_LBUTTONUP` 또는 캡처 상실 시 드래그 상태를 끝낸다.
- 세션 종료: 창 종료 또는 새 셸 시작 전에 셸에 정상 종료 명령을 보내고 짧게 완료를 기다린다. 그 다음 writer를 닫고, child가 계속 살아 있으면 강제 종료 fallback을 수행하되 child wait는 polling timeout으로 제한해 UI thread가 영구 블로킹되지 않게 한다. 이후 master handle과 reader thread를 정리한다.
- 카테고리 선택: `WM_COMMAND`의 콤보박스 `CBN_SELCHANGE` 알림을 선택 인덱스로 읽고, 앱 계층의 `CommandPanel` 선택 카테고리를 갱신한 뒤 오른쪽 버튼 컨트롤 목록을 다시 동기화한다.
- 명령 버튼 목록 스크롤: 버튼 수가 현재 명령 버튼 목록 영역에 표시 가능한 개수를 넘으면 infra 계층은 Win32 ScrollBar 컨트롤을 표시한다. `WM_VSCROLL` 또는 마우스 휠 입력은 버튼 목록의 시작 인덱스를 변경하고, 도메인 `WindowLayout`은 새 시작 인덱스에서 보이는 버튼만 배치한다.
- 카테고리 관리: 카테고리 콤보박스 우클릭에서 팝업 메뉴를 열고 새 카테고리 생성, 현재 카테고리 이름 변경, 현재 카테고리 삭제, 카테고리 위/아래 이동, 현재 카테고리에 버튼 추가를 실행한다. 새 카테고리 생성과 이름 변경은 Win32 모달 이름 입력창으로 처리하며, 빈 이름과 제어 문자가 포함된 이름은 복구 가능한 입력 오류로 처리한다. 마지막 카테고리 삭제는 복구 가능한 앱 오류로 처리한다.
- 버튼 관리: 명령 버튼 우클릭에서 팝업 메뉴를 열고 명령 실행, 버튼 편집, 버튼 삭제, 버튼 위/아래 이동을 실행한다. 삭제는 확인 후 현재 런타임 상태에서 제거한다.
- 버튼 편집: 새 버튼 추가 또는 기존 버튼 편집 시 Win32 모달 편집창을 열고 버튼 이름, 실행 파일/명령, 실행 인자를 입력받는다. 실행 인자 입력 옆에는 토큰 삽입 버튼을 제공해 사용자가 토큰 문자열을 외우지 않아도 되게 한다. 저장 시 빈 버튼 이름, 빈 실행 파일/명령, 줄바꿈이 포함된 인자는 복구 가능한 입력 오류로 처리한다.
- ButtonCommand: 플랫폼 버튼 이벤트를 도메인 `CommandButtonId`로 변환한다. UI 어댑터는 현재 버튼의 `CommandArguments`를 확인해 `{selectfile}`, `{selectdir}`, `{inputtext}`에 필요한 실행 시 입력을 먼저 모으고, 도메인은 현재 셸의 `ShellCommandDialect`에 맞춰 최종 `CommandText`를 만든다. 앱 계층은 이 `CommandText`를 현재 `TerminalSession::write_input` 경로로 전송한다. 버튼 클릭은 터미널 화면 상태를 직접 수정하지 않으며, 셸이나 실행 중인 프로그램이 echo한 PTY 출력만 화면에 반영한다.
- 기본 버튼 명령: 기본 카테고리 `Default` 안에 Windows 기본 셸 기준 메뉴 라벨 `cd`, `dir`, `echo hello`, `cls`를 제공한다. 각 명령은 전송 시 `\r`을 포함해 즉시 실행된다. Linux POSIX 셸에서는 같은 사용자 기능이 나도록 해당 기본 버튼의 실행 명령을 `pwd`, `ls`, `echo hello`, `clear`로 매핑하되 메뉴 구조와 표시 라벨은 Windows 기준을 따른다.
- 터미널 포커스: 터미널 렌더링 영역 클릭 또는 버튼 명령 처리 후 메인 창으로 포커스를 돌려 키 입력이 터미널 입력으로 들어오게 한다.

## Shutdown lifecycle

- shutdown은 앱 계층 유스케이스다. 창 종료, 새 세션 시작 전 정리, backend 실패 후 복구 정리는 모두 `TerminalSession::shutdown`으로 들어간다.
- `TerminalSession::shutdown`은 idempotent해야 한다. `Empty` 또는 `Exited` 상태에서는 추가 I/O 없이 종료 완료 상태를 유지하고, `Running` 또는 `Failed` 상태에서는 `ShuttingDown`을 거쳐 backend 정리를 수행한다.
- 종료 순서는 PTY 입력으로 정상 종료 명령 전송, child process의 짧은 종료 대기, writer drop, 필요 시 강제 종료 fallback, PTY master handle drop, reader thread 완료 대기와 join 순서다. master handle은 reader join 전에 해제해 blocking read가 EOF 또는 오류로 빠져나올 수 있게 한다.
- reader thread join은 무기한 대기하지 않는다. reader 완료 신호를 제한 시간 안에 받지 못하면 shutdown 실패를 `Result`로 반환하고 내부 원인을 남겨 UI thread deadlock을 피한다.
- child process가 정상 종료 요청 후에도 살아 있으면 fallback 종료를 요청하고, 이후에도 제한 시간 안에 종료되지 않으면 shutdown 실패를 `Result`로 반환하되 writer/master 등 다른 리소스 정리는 계속 시도한다.
- Win32 창 종료는 `WM_DESTROY`에서 shutdown을 실행하고 timer를 해제한 뒤 `PostQuitMessage`로 메시지 루프를 끝낸다. `WM_NCDESTROY`에서 `GWLP_USERDATA`에 저장한 `WindowState` 소유권을 회수해 Box를 drop한다.
- GDI 리소스는 획득한 범위에서 해제한다. `GetDC`/`ReleaseDC`, `BeginPaint`/`EndPaint`, `SelectObject` 복원, `CreateSolidBrush`/`DeleteObject`는 실패 반환 경로에서도 짝이 맞아야 한다.
- 사용자에게 보여주는 오류는 짧은 user message만 터미널 화면에 표시한다. 내부 원인, OS 오류 코드, I/O 원문은 `last_error` 또는 `TerminalFailure::cause`에 보관하고 사용자 표시 메시지와 분리한다.

## Win32 창 구조

- `WM_CREATE`: `CreateWindowExW`의 생성 파라미터로 전달된 창 상태를 `GWLP_USERDATA`에 등록하고, 오른쪽 영역에 배치할 Win32 ComboBox와 Button 컨트롤을 만든 뒤 초기 레이아웃을 계산한다.
- 앱 아이콘 적용: `WM_CREATE` 흐름에서 실행파일에 포함된 `ApplicationIcon` 리소스를 big/small 크기로 로드하고 `WM_SETICON`으로 창에 적용한다. 아이콘 핸들은 창 상태가 소유하며 창 소멸 시 해제한다.
- `WM_SIZE`: 새 클라이언트 크기로 `WindowLayout`을 다시 계산하고 탭 배치, 카테고리 콤보박스, 버튼 목록 스크롤바와 버튼 컨트롤 위치, 터미널 행/열을 갱신한다. 행/열 계산은 렌더러가 측정한 고정폭 폰트 셀 크기를 기준으로 한다.
- `WM_PAINT`: GDI 렌더러가 탭 바와 현재 활성 탭의 `TerminalViewport`를 그린다. 터미널 배경을 지우고 터미널 콘텐츠 영역에 셀 단위 advance를 지정한 GDI 텍스트 출력으로 행 텍스트를 그린 뒤 커서 셀을 블록으로 표시한다. 행 텍스트와 커서는 터미널 콘텐츠 영역 안으로 클리핑하며, 마지막 행 아래 남는 픽셀 영역은 배경으로 유지한다. 한글 같은 wide 문자와 뒤따르는 spacer 셀이 있어도 화면 위치, 선택 배경, 복사 범위는 같은 터미널 셀 좌표를 기준으로 맞춰야 한다. paint 오류는 내부 `last_error`에만 기록하고, paint 경로에서 터미널 상태 모델에 사용자 메시지를 주입하지 않는다.
- 렌더러의 터미널 행 캐시는 현재 `TerminalViewport` 인스턴스 안에서만 유효하다. 탭 전환, viewport 교체, rollback restore, viewport clear가 발생하면 GDI 행 캐시를 폐기해 다른 탭의 같은 row version을 재사용하지 않는다. 같은 row version이라도 현재 행 셀 내용과 캐시 내용이 다르면 캐시를 재사용하지 않아 빈 행에 이전 출력이 다시 그려지지 않게 한다.
- `WM_COMMAND`: 카테고리 콤보박스의 `CBN_SELCHANGE` 알림은 선택 카테고리 변경으로 처리한다. 실제 명령 버튼 child HWND에서 온 `BN_CLICKED` 알림만 버튼 컨트롤 ID를 명령 버튼 ID로 변환하고, 필요한 실행 시 입력을 모은 뒤 앱 세션에 최종 `CommandText` 실행을 위임한다. 알 수 없는 ID, 메뉴/액셀러레이터성 command, 비클릭 알림은 버튼 명령으로 처리하지 않는다.
- `WM_VSCROLL`/`WM_MOUSEWHEEL`: 명령 버튼 목록 스크롤 입력이면 현재 버튼 목록의 시작 인덱스를 변경하고 버튼 컨트롤 배치를 다시 적용한다. 터미널 스크롤 입력이면 활성 탭의 scrollback 표시 위치를 바꾸고 터미널 viewport와 스크롤바 정보를 다시 동기화한다.
- `WM_CONTEXTMENU`: 카테고리 콤보박스와 명령 버튼 child HWND에서 온 우클릭 요청을 팝업 메뉴로 처리한다. 메뉴 ID는 Win32 어댑터 내부에만 두고 앱 계층에는 도메인 명령으로 전달한다. 폰트 설정 항목은 `ChooseFontW` 공용 대화상자를 열어 고정폭 폰트와 크기를 선택하게 하고, About 항목은 버전과 클릭 가능한 GitHub 링크를 포함한 정보 창을 연다.
- `WM_CHAR`: `TranslateMessage`가 만든 UTF-16 문자 입력을 `TerminalInput`으로 변환한다. 일반 문자는 UTF-8 바이트로 전달하고, Ctrl+C 같은 C0 제어 문자는 해당 제어 바이트로 전달한다. surrogate pair는 Win32 어댑터에서 하나의 Unicode scalar로 합친 뒤 도메인 변환 규칙에 넘긴다.
- `WM_KEYDOWN`/`WM_SYSKEYDOWN`: Enter, Backspace, Tab, Escape, 방향키처럼 키 의미가 중요한 입력을 `TerminalInput`으로 변환한다. Alt가 포함된 방향키는 `WM_SYSKEYDOWN`에서도 동일한 경로로 처리한다. 선택이 있는 `Ctrl+C`와 `Ctrl+V`는 클립보드 유스케이스로 먼저 처리하고, 처리된 키가 뒤이어 만드는 `WM_CHAR` 제어 문자는 중복 입력을 막기 위해 Win32 어댑터에서 무시한다.
- `WM_TIMER`: 모든 탭의 PTY 이벤트를 주기적으로 drain하고 활성 탭에 출력이 있으면 창을 invalidate한다.
- `WM_LBUTTONDOWN`: 영역 분할선, 탭 닫기, 새 탭, 탭 전환 hit target을 처리한다. 분할선 클릭은 마우스 캡처를 시작한다. 탭 UI가 아닌 터미널 콘텐츠 셀 클릭은 선택 드래그 캡처를 시작하고, 터미널 렌더링 영역 클릭은 메인 창 포커스를 되돌려 키 입력이 활성 탭으로 들어오게 한다.
- 탭 목록 또는 활성 탭이 바뀌면 Win32 뷰 어댑터는 `TerminalTabView`와 `WindowLayout`의 탭 배치 캐시를 함께 갱신하고, 터미널 viewport 캐시를 새 활성 탭 기준으로 교체한다. 렌더링과 hit-test는 `WindowLayout`의 `TabPlacement`를 읽고 터미널 렌더링은 캐시된 `TerminalViewport`를 읽기 때문에, 캐시가 분리되면 탭 전환 표시, 클릭 영역, 터미널 내용이 오래된 상태로 남을 수 있다.
- `WM_MOUSEMOVE`: 분할선 드래그 중이면 명령 버튼 영역 폭과 터미널 크기를 갱신한다. 터미널 선택 드래그 중이면 현재 포인터를 터미널 grid 범위로 clamp해 선택 focus를 갱신한다. 드래그 중이거나 포인터가 분할선 위에 있으면 좌우 크기 조절 커서를 표시한다.
- `WM_LBUTTONUP`: 진행 중인 분할선 드래그 또는 터미널 선택 드래그와 마우스 캡처를 종료한다.
- `WM_CAPTURECHANGED`: 외부 요인으로 마우스 캡처를 잃으면 분할선 드래그와 터미널 선택 드래그 상태를 정리한다.
- `WM_DESTROY`: timer를 해제하고 세션 종료 명령을 실행한 뒤 `PostQuitMessage`로 메시지 루프를 끝낸다.
- `WM_NCDESTROY`: `GWLP_USERDATA`를 비우고 `WindowState` Box를 회수해 Win32 창 상태 소유권을 정리한다.

## GTK4 창 구조

- `Application::activate`: `ApplicationWindow`와 루트 위젯을 구성하고, 기본 탭 PTY 세션을 시작한 뒤 실제 할당 크기에 맞춰 터미널을 resize한다.
- 화면은 같은 `WindowLayout` 도메인 계산을 사용한다. 왼쪽 터미널 영역과 탭/분할선 같은 custom-drawn UI는 `DrawingArea`로 그리고, 오른쪽 명령 패널의 GTK ComboBox/Button/Scrollbar 계열 위젯은 개별 `Overlay` child로 올려 빈 영역 포인터 이벤트가 `DrawingArea`에 도달하게 한다.
- `DrawingArea::set_draw_func`: Win32 GDI 렌더러와 같은 색상, 탭 배치, 터미널 셀, 커서, 선택 영역 규칙을 Cairo 기반으로 그린다. 저장된 `TerminalFont`로 Cairo 폰트를 선택하고 셀 메트릭을 측정한다.
- `glib::timeout_add_local`: Win32 `WM_TIMER`와 같은 PTY 이벤트 drain 주기와 저장 지연 flush 역할을 수행한다.
- `EventControllerKey`: 활성 GTK 창 수준에서 문자 입력, Enter/Backspace/Tab/Escape/방향키, `Ctrl+C`, `Ctrl+V`를 `TerminalInput` 또는 클립보드 유스케이스로 변환한다. 버튼/콤보/오류 처리 뒤에도 Windows 최상위 창 키 메시지 처리와 같이 터미널 입력 경로가 유지되어야 한다.
- `GestureClick`, `GestureDrag`, `EventControllerMotion`, `EventControllerScroll`: 탭 전환/닫기/추가, 터미널 선택 드래그, 분할선 드래그, 터미널/명령 버튼 목록 스크롤을 Win32와 같은 도메인 요청으로 변환한다. GTK에서 motion 이벤트가 버튼 누름 드래그 중 안정적으로 전달되지 않는 경우가 있어, `GestureDrag`의 drag update도 같은 드래그 상태 갱신 경로로 연결한다.
- 카테고리와 버튼 컨텍스트 메뉴는 GTK 모달 메뉴 창으로 제공한다. 우클릭은 위젯의 raw `ButtonRelease`/context-menu 이벤트를 잡아 release 뒤 메뉴 창을 띄우며, 메뉴 항목은 Win32 팝업 메뉴와 같은 실행, 편집, 삭제, 위/아래 이동, 폰트 설정, About 표시 동작을 호출한다.
- 명령 버튼 좌클릭은 기본적으로 GTK `Button::clicked`로 처리하되, Overlay 입력 라우팅 차이로 일부 visible 버튼의 clicked signal이 누락되는 경우에는 `DrawingArea`가 같은 `WindowLayout` hit-test로 visible `CommandButtonId`를 찾아 같은 실행 경로를 호출한다.
- 파일 선택, 폴더 선택, 텍스트 입력, 버튼 편집, 폰트 선택은 GTK 대화상자로 제공한다. 파일/폴더 선택은 응답이 앱 내부 모달 루프로 돌아오는 `FileChooserDialog`를 사용하고, 폰트 선택은 고정폭 family와 이름상 고정폭으로 식별되는 CJK family로 필터링한 `FontChooserDialog`를 사용한다. 저장 전 검증은 관련 도메인 규칙을 그대로 사용한다.

## 플랫폼 차이

- 기본 셸: Windows는 `ComSpec`, PowerShell, `cmd.exe` 순서로 감지하고 Linux는 `$SHELL`, `/bin/sh` 순서로 감지한다. 터미널 파서와 입력 경로는 UTF-8 byte stream을 기준으로 하므로 Windows `cmd.exe`는 시작 시 `chcp 65001`로 코드페이지를 UTF-8로 고정하고, PowerShell은 `[Console]::InputEncoding`, `[Console]::OutputEncoding`, `$OutputEncoding`을 UTF-8로 맞춘다. Linux POSIX PTY 셸은 GUI 실행 환경의 `TERM=dumb` 같은 값을 그대로 물려받으면 `clear` 등 터미널 제어 명령이 Windows `cls`와 기능 등가로 동작하지 않으므로 `TERM=xterm-256color`를 명시한다. 따라서 `{path}`와 `{name}`은 Linux에서 `"$PWD"`와 `"${PWD##*/}"` 계열 POSIX 셸 표현으로 변환한다.
- 기본 명령 버튼: Windows의 기본 버튼 라벨은 `cd`, `dir`, `echo hello`, `cls`이다. Linux도 새 설정 파일을 만들 때 같은 라벨을 사용하고, POSIX 셸에서 같은 기능이 실행되도록 내부 실행 명령만 `pwd`, `ls`, `echo`, `clear`로 매핑한다.
- 아이콘: Windows는 PE 리소스의 `icon.ico`를 창/실행 파일 아이콘으로 사용한다. Linux GTK4는 `io.github.edgarp9.j3Term` application id와 icon-name을 사용하며, `--install`이 `$XDG_DATA_HOME` 또는 `$HOME/.local/share` 아래에 desktop entry와 `icon.svg`/`icon.png`를 설치한다. 대문자가 있는 app id의 KDE/Plasma fallback을 위해 lowercase alias desktop entry와 아이콘도 함께 관리한다.
- 대화상자와 메뉴의 시각적 모양은 플랫폼 네이티브 컨트롤 차이로 완전히 같을 수 없다. 동작 결과와 도메인 상태 변화는 동일하게 유지한다. About 창의 링크는 GTK에서는 링크 버튼으로, Windows에서는 URL 텍스트 버튼으로 제공하며 클릭 시 기본 브라우저에 URL 열기를 위임한다.

## 책임 경계

- `entry`: 바이너리 진입점에서 현재 플랫폼 실행 흐름 또는 명시적 Linux desktop 통합 명령으로 위임한다.
- 시작 인자 파싱: `entry`가 `--install`/`--uninstall` 단독 인자는 desktop 통합 명령으로 분리하고, 그 외에는 첫 OS 인자의 폴더 여부를 확인해 시작 작업 디렉터리 옵션으로 분리한다. 명령으로 실행할 OS 인자는 앱 내부 문자열 인자로 변환한다. 명령 조합과 PTY byte 변환 규칙은 `domain`의 `StartupCommand`에 둔다.
- 빌드 스크립트: `icon.ico`를 Windows 실행파일 리소스에 포함해 Explorer와 작업 표시줄이 앱 아이콘을 인식할 수 있게 한다.
- 릴리즈 빌드 스크립트: `build_release.py`가 Cargo release 빌드 실행, target별 산출물 폴더 계산, 파일 관리자 열기를 담당한다. 앱 도메인 로직이나 플랫폼 런타임 로직에는 의존하지 않는다.
- `app`: 터미널 세션과 탭 유스케이스, 명령 실행, 상태 전이를 관리한다.
- `domain`: 명령 패널, 카테고리, 명령 버튼, 입력 이벤트, 터미널 이벤트, 탭 식별/표시 모델, 세션 상태, 이벤트별 상태 전이 규칙, 크기 규칙, UI 영역/레이아웃 개념을 정의한다.
- `infra::config`: 실행 파일 옆 TOML 설정 파일 경로 계산, 읽기/쓰기, TOML DTO와 도메인 `AppSettings` 사이 변환을 담당한다.
- `infra::linux_desktop`: Linux 사용자 영역 desktop entry, hicolor 아이콘, KDE/Plasma lowercase alias, legacy id 제거, desktop/icon 캐시 갱신 명령 실행을 담당한다.
- `infra::pty`: `portable_pty`로 셸 프로세스와 PTY I/O를 담당한다.
- `infra::terminal`: `alacritty_terminal` parser와 grid/screen 모델을 감싸고 `TerminalViewportPort`를 구현한다. 외부 크레이트 타입은 이 어댑터 내부에 고립하고 app/domain에는 `TerminalViewport`, `TerminalCell`, `CursorPosition`만 반환한다.
- `infra::win32`: 창 등록, 앱 아이콘 로드/적용, Win32 메시지 루프, ComboBox/Button 컨트롤 생성, 팝업 메뉴 처리, Win32 메시지를 도메인 입력으로 넘기기 위한 가상키/문자 추출, 크기 이벤트를 담당한다.
- `infra::gtk`: GTK4 애플리케이션/창/위젯 구성, GTK 입력 이벤트를 도메인 입력으로 넘기기, GTK 대화상자/클립보드/그리기/크기 이벤트를 담당한다.
- `infra::renderer`: Win32 GDI 기반 기본 텍스트 렌더링을 담당하며 탭 바와 터미널 렌더링 영역 좌표를 기준으로 활성 탭의 `TerminalViewport` 스냅샷을 그린다. 저장된 `TerminalFont`를 GDI font handle로 변환하고 DPI별 셀 메트릭을 측정한다.

## TerminalInput key mapping

- 일반 문자: `WM_CHAR`의 Unicode scalar를 UTF-8로 인코딩해 PTY writer에 전달한다.
- Enter: `WM_KEYDOWN`의 `VK_RETURN`을 `0x0D` (`\r`)로 전달한다.
- Backspace: `WM_KEYDOWN`의 `VK_BACK`을 `0x7F` (DEL)로 전달한다. Windows ConPTY의 일반 line editing과 맞추기 위한 터미널 Backspace 키 기본값이다.
- Tab: `WM_KEYDOWN`의 `VK_TAB`을 `0x09` (`\t`)로 전달한다. Shift+Tab은 `ESC [ Z`로 전달한다.
- Escape: `WM_KEYDOWN`의 `VK_ESCAPE`을 `0x1B`로 전달한다.
- Ctrl+C: Windows가 `WM_CHAR`로 전달하는 `U+0003`을 `0x03`으로 전달한다. 다른 Ctrl 문자 조합도 C0 제어 문자로 들어오면 같은 규칙으로 전달하되, Enter/Backspace/Tab/Escape와 중복되는 `U+000D`, `U+0008`, `U+0009`, `U+001B`는 keydown 처리와 중복되지 않게 무시한다.
- 방향키: `WM_KEYDOWN` 또는 `WM_SYSKEYDOWN`의 방향키 가상키를 VT CSI sequence로 변환한다.
- 방향키 기본값: Up `ESC [ A`, Down `ESC [ B`, Right `ESC [ C`, Left `ESC [ D`.
- 방향키 modifier: Shift/Alt/Ctrl이 있으면 xterm 규칙에 따라 `ESC [ 1 ; n <final>`을 사용한다. `n`은 `1 + Shift(1) + Alt(2) + Ctrl(4)`이며 예를 들어 Ctrl+Left는 `ESC [ 1 ; 5 D`이다.

## 남은 설계 포인트

- PTY reader thread는 blocking read를 사용하므로 종료는 child process 종료와 PTY handle 정리에 의존한다. shutdown 경로에서는 child를 먼저 종료하고 reader thread를 join한다.
- 렌더링은 기본 전경/배경색, 블록 커서, 선택 영역만 반영한다. 셀별 색상, 스타일, IME 조합 표시는 아직 반영하지 않는다.
- 설정 파일 포맷은 현재 `version = 1`만 지원하며 명령 패널 필드와 `[font]` 섹션을 포함한다. 향후 호환되지 않는 필드가 늘어나면 마이그레이션 규칙을 별도로 추가해야 한다.
- 명령 버튼은 Win32 Button 컨트롤로 연결했지만, 활성/비활성 상태와 버튼별 실패 표시 UI는 아직 없다.
