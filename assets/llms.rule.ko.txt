# wtree rule — 규칙 파일 레퍼런스

규칙은 `.git/wtree/rules`에 정의한다. 텍스트 편집으로 수정하고, 다음 wtree 실행부터 반영된다. 'wtree rule'로 현재 적용되는 규칙을 확인할 수 있다.

## 예시

main-dev 형태 예시:

```ini
[main]
children = dev
destroyable = false
merge-mode = ff, no-ff

[dev]
children = group:work
merge-mode = squash

[group:work]
name-allow = feat/*, fix/*
```

다음과 같이 작동한다.
- main에서 'wtree new' 로 dev 브랜치를 만들 수 있다.
- dev에서 'wtree new' 로 [group:work] 브랜치를 만들 수 있다. 이름은 `feat/*`, `fix/*` 가 아니면 거부된다.
- dev 브랜치는 'wtree merge'로 main에 머지할 수 있다. merge 방법은 ff, no-ff 중 선택할 수 있다.
- [group:work] 브랜치는 'wtree merge'로 dev에 머지할 수 있다. merge 방법은 squash로 한정된다.
- main은 'wtree destroy'로 제거할 수 없다.

## 작성법

### 섹션

`[X]`는 고정 브랜치 하나를 선언한다. `[group:X]`는 같은 정책을 받는 작업 브랜치 묶음을 선언한다. `children`이 부모-자식 관계를 정한다. 자식으로 선언된 브랜치는 그 섹션의 브랜치를 부모로 가진다.

### 키

- children (`[X]` `[group:X]`): 이 섹션을 부모로 삼을 수 있는 것. `group:X`, 선언된 고정 브랜치 이름, `*`(자유 브랜치)를 쉼표로 나열한다. 선언된 고정 브랜치 이름은 `[X]` 섹션에서만 나열할 수 있다.
- destroyable (`[X]`): `false`면 destroy를 무조건 거부한다. 기본값 `true`.
- name-allow / name-deny (`[group:X]`): 브랜치 이름의 glob 패턴. `*`와 `?`만 특수 문자다.
- ephemeral (`[group:X]`): `true`면 부모를 destroy할 때 안전 조건을 통과한 자식이 함께 삭제된다. 기본값 `false`.
- merge-mode (`[X]` `[group:X]`): 이 브랜치가 받아들이는 병합 방식. squash, rebase, no-ff, ff를 쉼표로 나열한다. 기본값은 모든 머지 허용. 머지 방식이 하나라면 `wtree merge`에서 옵션을 생략할 수 있다. none은 단독으로만 사용할 수 있으며 어떤 머지도 허용하지 않는다.
- copy (`[X]` `[group:X]`): 새 워크트리가 부모 워크트리에서 가져올 미추적 파일. 패턴은 워크트리 루트의 항목과 대조하며, 디렉터리는 끝에 `/`를 붙인다.
- description (`[X]` `[group:X]`): 이 브랜치가 무엇을 위한 것인지 한 줄. `wtree`와 `wtree info`가 출력한다.

### merge-mode의 동작 방식

`no-ff`
- 부모에서 'git merge --no-ff'를 수행한 것과 동일하다. git과 달리 브랜치도 머지 커밋으로 전진한다.
- `-m`(커밋 메시지)가 요구된다.

`ff`
- 부모에서 'git merge --ff-only'를 수행한 것과 동일하다.

`rebase`
- 2단계로 작동한다.
    1. 'git rebase parent'
    2. 부모에서 'git merge --ff-only child' 수행

`squash`
- 4단계로 작동한다.
    1. 머지 베이스 이후 커밋에 대해 soft reset
    2. 하나의 변경사항으로 커밋
    3. 'git rebase parent'
    4. 부모에서 'git merge --ff-only' 수행
- 따라서 기존 squash와 달리 브랜치를 계속 활용할 수 있다.
- `-m`(커밋 메시지)가 요구된다.

`none`
- 어떤 병합도 허용하지 않는다.
