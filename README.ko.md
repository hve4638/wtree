# wtree

[English](README.md)

wtree는 정책 기반 git worktree 관리 도구로, AI 에이전트와 함께 쓰는 것을 전제로 설계되었다.

## 아이디어

`git worktree`는 여러 브랜치를 작업하기 좋은 기능이며, 특히 Claude Code나 Codex 같은 AI 에이전트를 병렬로 사용할 때 각각의 고유한 작업 공간을 제공할 수 있다.

그러나 경로를 직접 정해야 하며, 다 쓴 뒤에도 워크트리 제거 및 브랜치 삭제가 번거롭다.

에이전트를 쓰면 브랜치를 어디서 따고 어디로 병합할지가 에이전트에게 준 지식에만 달린다. 에이전트는 그 규칙을 종종 어기고, 어겼다는 사실을 돌려받지도 못한다.

wtree는 정책 구성을 통해 워크트리를 다룰 때 부모 관계, 네이밍, 머지 방법(ff, rebase, squash 등)을 미리 정해두고 그 안에서의 동작만 허용하도록 한다.

## 설치

```bash
cargo install gitwtree
```

명령어는 `wtree`다. Unix 계열 운영체제만 지원한다.

## 시작하기

```bash
wtree init
```

`.git/wtree/config`가 생긴다. 열어서 구조를 선언한다. main 아래에 작업 브랜치를 두는 최소 형태:

```ini
[main]
children = group:work
destroyable = false
merge-mode = squash

[group:work]
name-allow = feat/*, fix/*
ephemeral = true
```

이제 브랜치를 만든다.

```bash
wtree new feat/login
```

새 워크트리로 이동할 `cd` 명령이 출력된다. 거기서 작업하고 커밋한 뒤,

```bash
wtree land -m "feat: add something"
```

main으로 squash 병합하고 워크트리와 브랜치를 정리한다. 워크트리를 남기고 병합만 하려면 `wtree merge`를 쓴다. `squash`와 `no-ff`는 새 커밋을 만들기 때문에 `-m`이 필요하다.

`wtree`만 실행하면 현재 워크트리에서 정책을 통과할 동사만 보여준다. 전체 매뉴얼은 `wtree help --all`이다.

## 동사

| 동사 | 설명 |
|---|---|
| `new <name>` | 브랜치와 워크트리를 생성 |
| `open <branch>` | 기존 브랜치에 워크트리를 연결 |
| `close` | 브랜치는 두고 워크트리만 제거 |
| `merge` | 자기 부모로 병합 |
| `sync` | 부모 브랜치의 변경을 현재 브랜치로 병합 |
| `land` | 병합한 뒤 destroy |
| `destroy` | 브랜치와 워크트리를 제거 |
| `adopt` | 기존 브랜치를 정책 아래로 편입 |
| `list` / `info` | 무엇이 있고 여기서 무엇이 허용되는지 표시 |
| `init` | 초기 설정 파일을 생성 |

## 설정

| 파일 | |
|---|---|
| `config` | branch 정책 |
| `settings` | 워크트리 생성 위치 등 설정 |
| `hooks/post-create` | `wtree new` 직후 실행되는 훅 |

`config`에서 `[X]`는 고정 브랜치, `[group:X]`는 같은 정책을 적용받는 작업 브랜치 그룹이다. `children`은 이 섹션을 부모로 삼을 수 있는 브랜치를 선언한다.

| 키 | 사용 가능 섹션 | 뜻 |
|---|---|---|
| `children` | `[X]` `[group:X]` | `group:X`, `*` (자유 브랜치), 선언된 고정 브랜치 이름 |
| `destroyable` | `[X]` | `false`면 destroy를 무조건 거부. 기본값 `true` |
| `name-allow` / `name-deny` | `[group:X]` | 브랜치 이름 glob 패턴 (`*`, `?`만 특수) |
| `ephemeral` | `[group:X]` | 부모를 destroy할 때 안전 조건을 통과하면 함께 삭제. 기본값 `false` |
| `merge-mode` | `[X]` `[group:X]` | 이 브랜치가 받아들이는 병합 방식. `squash`, `rebase`, `no-ff`, `ff`를 쉼표로 나열 |
| `copy` | `[X]` `[group:X]` | 부모 워크트리에서 새 워크트리로 복사할 추적되지 않은 파일 |

## 라이선스

MIT