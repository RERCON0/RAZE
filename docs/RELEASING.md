# Подписанный релиз RAZE

Релизный пакет содержит только `raze.exe`, `raze-app.exe`, `raze_shell.dll`,
`RAZE.msix`, `RAZE.cer`, README, GPL, уведомление о лицензии шрифта,
`manifest.json`, `manifest.sig` и публичный ключ.
Приватный ключ в архив не попадает. Проверка ZIP не извлекает и не запускает EXE.

## Подготовка

Нужны Windows x64, MSVC Build Tools + Windows SDK (`makeappx.exe`, `signtool.exe`),
Rust (stable) и `windres` (MSYS2/MinGW-w64), Python 3.13+ и OpenSSL 3
(входит в Git for Windows). Cargo.lock обязателен. Сначала выполните проверки
из корня репозитория:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --lib --workspace
cargo audit --deny yanked --deny unsound
```

Закоммитьте изменения: сборщик требует чистый закоммиченный checkout.

## Ключ издателя

У RAZE собственный Ed25519-ключ, отдельный от других проектов.
Доверенный публичный ключ — [`release/public-key.pem`](../release/public-key.pem).
Приватный ключ владельца проекта хранится в
`%LOCALAPPDATA%\RAZE\release-signing\raze-private.pem`, вне рабочего дерева.
Каталог ограничен ACL текущего пользователя. Резервную копию приватного ключа
нужно хранить отдельно; его потеря потребует публично объявить смену ключа.

Для своего форка создайте отдельную пару, предварительно задав новый путь
публичного ключа. Команда не перезаписывает существующие ключи:

```powershell
python scripts/release.py keygen --private-key "$env:LOCALAPPDATA\MyFork\release-signing\fork-private.pem" --public-key .\release\fork-public.pem
```

Перед сборкой форка замените доверенный `release/public-key.pem` новым
публичным ключом и опубликуйте его отпечаток в документации форка.

## Сборка

```powershell
python scripts/release.py build --private-key "$env:LOCALAPPDATA\RAZE\release-signing\raze-private.pem"
python scripts/release.py verify .\dist\raze-windows-x64.zip
Get-FileHash .\dist\raze-windows-x64.zip -Algorithm SHA256
```

Сборщик сам запускает `packaging\build-package.ps1`: тот собирает разреженный
пакет `RAZE.msix` с `RAZE.cer` и обновляет вшитый отпечаток сертификата. Затем
в новом временном дереве `target` выполняется `cargo build --locked --release
--workspace` для MSVC x64, и проверяются оба EXE: архитектура, подсистемы
CLI/GUI, ASLR, DEP и статический CRT без `VCRUNTIME`/`MSVCP`. После компиляции
все входные файлы и Git-ревизия проверяются повторно.

Манифест содержит версию продукта, commit/tree Git, SHA-256 всех отслеживаемых
исходных файлов как агрегированный отпечаток, хеш Cargo.lock, версии Cargo/Rust,
размеры и SHA-256 всей полезной нагрузки, свойства PE и отпечаток публичного
ключа. Ed25519 подписывает точные канонические байты манифеста. Новый ZIP
проверяется тем же верификатором до атомарной замены прежнего архива.

`RAZE.msix` дополнительно подписан самозаверяющим сертификатом
(CN=RERCON RAZE, отпечаток вшит в `raze.exe`): `raze --install` пинит его через
`packaging\cert-thumbprint.txt`. Это Authenticode-подпись пакета для Windows,
она не заменяет Ed25519-подпись архива. EXE не подписаны.

Вместе с релизом опубликуйте commit и SHA-256 архива; исходный код этого
commit доступен по URL из манифеста (после push).

## Проверка скачанного ZIP

Используйте ключ и скрипт из доверенного checkout RAZE:

```powershell
python scripts/release.py verify C:\Downloads\raze-windows-x64.zip
```

Скрипт отклоняет неправильную подпись/ключ, несовпадение хешей, неизвестные
поля манифеста, лишние/пропущенные/повторные ZIP-члены, симлинки, слишком
большие данные и усечённые архивы. Валидация происходит без распаковки.
Доверенный ключ нельзя брать только из самого ZIP.

Подпись пакета подтверждает издателя и целостность файлов. Это не сертификат
Authenticode и не отмена предупреждений SmartScreen.
