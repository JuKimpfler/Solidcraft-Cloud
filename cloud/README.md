# SolidCraft_Cloud

Fork von [SolidCraft](https://github.com/tangency-labs/solidcraft) (Open-Source-Parametrik-CAD in
Rust), mit dem ein kleines Team (bis 3 Personen) gemeinsam an 3D-Modellen arbeitet. Gedacht für
einen Raspberry Pi 5 hinter Apache.

> **Unabhängiges Projekt.** Dieser Fork gehört nicht zu den SolidCraft-Autoren und wird von ihnen
> weder betrieben noch unterstützt. SolidCraft steht unter MIT OR Apache-2.0, die Lizenzdateien
> und Hinweise (`LICENSE-*`, `NOTICE`, `ATTRIBUTION.md`) bleiben unverändert.

## Ziel

- Designs liegen **auf dem Server**, nicht im Browser oder auf der lokalen Festplatte, und sind in
  allen Instanzen gleich.
- Wer ein Design öffnet, **sperrt** es für die anderen. Die anderen bekommen eine Kopie und einen
  Hinweis.
- Die Startseite zeigt die Server-Designs statt der eingebauten Beispiele.
- Es läuft immer eine **aktuelle SolidCraft-Version**: Upstream-Änderungen werden regelmäßig in
  diesen Fork übernommen.

## Aufbau

Unser Code liegt in **`cloud/`**, damit er sich nicht mit Upstream-Dateien überschneidet.
Änderungen an SolidCraft selbst sind bewusst klein und auf wenige Dateien beschränkt
(`apps/solidcraft-web/src/`, `crates/ui-egui/src/home.rs`). Das README im Wurzelordner und
`AGENTS.md` bleiben Upstream-Dateien und werden von uns nicht geändert.

| Pfad | Inhalt |
|---|---|
| `cloud/README.md` | Diese Übersicht |
| `cloud/ARCHITECTURE.md` | Entwurf: Schnittstelle, Sperren, Updates |
| `cloud/server/` | Der Datei-Server (Liste, Laden, Speichern, Sperren), siehe dessen README |

## Stand

| # | Meilenstein | Status |
|---|---|---|
| M0 | Fork, Projektgerüst, unverändertes Web-Image auf dem Pi | Image baut in CI, Test auf dem Pi steht aus |
| M1 | Datei-Server | fertig und getestet, Einsatz auf dem Pi steht aus |
| M2 | Client-Anbindung (Öffnen/Speichern über den Server) | offen |
| M3 | Sperren in der App | offen |
| M4 | Startseite mit Server-Designs | offen |
| M5 | Automatische Updates (Upstream-Abgleich) | offen |

## Web-Image (M0)

Der Fork enthält schon SolidCraft's eigenen Workflow `.github/workflows/docker.yml`. Er baut die
Web-App und veröffentlicht sie für `linux/amd64` und `linux/arm64` (Raspberry Pi):

```
ghcr.io/jukimpfler/solidcraft-web:edge     letzter Stand von main
ghcr.io/jukimpfler/solidcraft-web:<version>  bei veröffentlichten Releases
```

Einrichten:

1. Im Fork unter **Actions** die Workflows aktivieren. Nur **Docker** wird gebraucht; die übrigen
   (Release, Pages, Winget, Snap, Triage, …) sind für Upstream gedacht und können unter
   *Actions → Workflow → ⋯ → Disable workflow* abgeschaltet werden.
2. **Docker** einmal von Hand starten (*Run workflow*, Branch `main`).
3. Nach dem ersten Lauf unter *Packages* das Paket `solidcraft-web` auf **Public** stellen.
4. Auf dem Pi:
   `docker run -d --name solidcraft -p 127.0.0.1:8080:8080 --restart unless-stopped ghcr.io/jukimpfler/solidcraft-web:edge`
5. Apache leitet eine Subdomain mit TLS an `127.0.0.1:8080` weiter. Speichern im Browser braucht
   HTTPS oder `localhost`.

## Upstream übernehmen

```sh
git remote add upstream https://github.com/tangency-labs/solidcraft.git   # einmalig
git fetch upstream
git checkout -b sync/upstream main
git merge upstream/main          # Konflikte nur in den wenigen Dateien, die wir ändern
```

Danach als Pull Request in den eigenen `main`. Nie in einen Pull Request gegen
`tangency-labs/solidcraft` mischen: Unsere Pull Requests gehen immer gegen **diesen** Fork.

## Regeln für Änderungen an SolidCraft-Code

Aus `AGENTS.md`, soweit sie uns betreffen:

- Kein `unwrap()`, `expect()`, `panic!`, `todo!`, kein `unsafe` im Anwendungscode. Fehler laufen
  über `Result`.
- Alles unterhalb der Apps muss für `wasm32-unknown-unknown` bauen (`cargo xtask wasm`).
- Vor jedem Commit: `cargo xtask ci` (fmt, clippy, Tests, Assets, Schichten).
- Neue Dateien (Bilder, Schriften) brauchen einen Eintrag in `ATTRIBUTION.md`.
- Anwendungscode ist Rust. Der Datei-Server in `cloud/server/` ist deshalb ebenfalls in Rust, als
  eigenständiges Projekt außerhalb des Workspaces, damit die strengen Lint-Regeln und
  Upstream-Änderungen am Workspace ihn nicht stören.
