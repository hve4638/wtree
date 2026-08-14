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
wtree init --new
```

`.git/wtree/rules`가 생긴다. 열어서 구조를 선언한다. main 아래에 작업 브랜치를 두는 최소 형태:

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
| `init --new` | 초기 설정 파일을 생성 |
| `init --load [path]` | 대신 `.wtree/`에서 규칙을 가져옴 |
| `save [path]` | 규칙을 커밋할 수 있는 `.wtree/`로 복사 |

`wtree init`에 두 플래그를 다 주지 않으면 어느 쪽인지 묻고, 물어볼 터미널이 없으면 거부한다.

## 설정

| 파일 | |
|---|---|
| `rules` | branch 정책 |
| `settings` | 워크트리 생성 위치 등 설정 |
| `hooks/` | `new`, `merge`, `destroy` 전후에 실행되는 스크립트 |

`rules`에서 `[X]`는 고정 브랜치, `[group:X]`는 같은 정책을 적용받는 작업 브랜치 그룹이다. `children`은 이 섹션을 부모로 삼을 수 있는 브랜치를 선언한다.

| 키 | 사용 가능 섹션 | 뜻 |
|---|---|---|
| `children` | `[X]` `[group:X]` | `group:X`, `*` (자유 브랜치), 선언된 고정 브랜치 이름 |
| `destroyable` | `[X]` | `false`면 destroy를 무조건 거부. 기본값 `true` |
| `name-allow` / `name-deny` | `[group:X]` | 브랜치 이름 glob 패턴 (`*`, `?`만 특수) |
| `ephemeral` | `[group:X]` | 부모를 destroy할 때 안전 조건을 통과하면 함께 삭제. 기본값 `false` |
| `merge-mode` | `[X]` `[group:X]` | 이 브랜치가 받아들이는 병합 방식. `squash`, `rebase`, `no-ff`, `ff`를 쉼표로 나열 |
| `copy` | `[X]` `[group:X]` | 부모 워크트리에서 새 워크트리로 복사할 추적되지 않은 파일 |
| `description` | `[X]` `[group:X]` | 이 브랜치가 무엇을 위한 것인지 한 줄. `wtree`와 `info`가 출력한다 |

`merge-mode`별로 부모에 남는 것:

| 모드 | 부모가 받는 것 | 브랜치의 커밋 |
|---|---|---|
| `ff` | 브랜치 커밋 그대로 | 유지 |
| `rebase` | 같은 커밋을 부모 tip 위에 재생 | 유지, 부모가 움직였으면 해시는 새로 |
| `squash` | 커밋 하나 | 그 하나로 접힘 |
| `no-ff` | 머지 커밋 하나 | 유지, 두 번째 부모에 |

`no-ff`는 아무것도 버리지 않으면서 squash처럼 읽힌다. `git log --first-parent`는 브랜치당 한 줄을 보여주고, 그냥 `git log`에는 커밋이 전부 남아 있다.

## 훅

`hooks/` 안의 실행 파일이고, 이름이 곧 실행 시점이다. `init`이 전체 계약을 설명하는 `post-create.sample`을 써 두므로 이름만 바꾸면 켜진다. 여러 이름으로 링크해두고 `$WTREE_HOOK`으로 분기해도 된다.

| 훅 | 실행 시점 |
|---|---|
| `pre-create` / `post-create` | `wtree new`과 `wtree open` 전후 |
| `pre-merge` / `post-merge` | `wtree merge`와 `land`의 병합 단계 전후 |
| `pre-destroy` / `post-destroy` | `wtree destroy`와 `land`의 삭제 단계 전후 |

`pre-` 훅은 관문이다. 0이 아닌 종료 코드는 아무것도 건드리기 전에 동사를 중단시킨다. `post-` 훅은 보고만 하므로 0이 아닌 종료 코드는 경고로 끝나고 동사가 한 일은 그대로 남는다. `sync`와 `close`는 훅을 실행하지 않는다. `land`에서는 두 관문이 모두 병합 전에 실행되므로 어느 쪽이든 동사 전체를 중단시킬 수 있다.

훅은 워킹트리를 원래대로 두고 나와야 한다. 훅이 새 파일을 남기면 `land`는 그것을 지우는 대신 멈춘다(`stopped:`, 파일과 실행된 훅을 나열). 파일을 처리한 뒤 `wtree destroy`로 마무리하면 된다.

모든 훅이 `WTREE_HOOK`, `WTREE_REPO`, `WTREE_INTERACTIVE`를 받고, 대상 워크트리에 대한 `WTREE_PATH`와 `WTREE_BRANCH`가 따라온다. `WTREE_VERB`는 실제로 타이핑된 동사라, create 쌍은 이것으로 `new`와 `open`을, 나머지 둘은 단독 동사와 `land`를 구분한다. 병합 훅에는 `WTREE_TARGET`, `WTREE_MODE`, `WTREE_MESSAGE`, `WTREE_DIRTY`가, `post-merge`에는 `WTREE_TIP`이 추가된다. 전체 목록은 샘플에 있다.

`new`와 `open`에서 `--` 뒤의 모든 것은 create 쌍에 `"$@"`로 도착한다 — 단어 경계 그대로, 아무것도 확장되지 않은 채. 워크트리를 만든 목적이 무엇이든 훅이 그것을 바로 시작할 수 있다.

```sh
wtree new feat/login -- claude 'fix GH #322'
```

`--no-hooks`는 그 실행에 한해 `pre-` 훅을 포함한 모든 훅을 건너뛴다. 훅 파일을 잠시 꺼놓고 되돌리는 걸 잊는 상황을 대신한다.

```sh
wtree merge --squash -m 'fix the thing' --no-hooks
```

## 라이선스

MIT