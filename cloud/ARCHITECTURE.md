# Architektur (Entwurf)

Stand: Meilenstein M0. Alles ab hier ist Plan und wird mit M1 bis M4 konkretisiert.

## Überblick

```
Browser (SolidCraft-Web, wasm, mit unseren Änderungen)
   │  HTTPS, gleiche Domain
   ▼
Apache auf dem Pi (TLS, Basic Auth)
   ├── /        → Container solidcraft-web (nginx, statische App)
   └── /api/    → Datei-Server (Liste, Laden, Speichern, Sperren)
                     └── Ordner, in Nextcloud als lokaler externer Speicher eingebunden
```

Es gibt nur eine Wahrheit: den Ordner auf dem Server. Eine eigene Synchronisation ist nicht
nötig, alle Instanzen lesen und schreiben dasselbe.

## Wo wir in SolidCraft eingreifen

Die Web-Hülle (`apps/solidcraft-web`, etwa 2400 Zeilen) ist dünn: Die Engine spricht nur mit einem
virtuellen Dateisystem (`vfs`), und die Hülle leitet Schreibzugriffe nach Pfad-Präfix weiter
(`/opfs/`, `/disk/`, `/downloads/`, siehe `disk.rs` und `files.rs`). Ein neues Präfix `/server/`
lässt sich dort einhängen, ohne den CAD-Kern anzufassen.

Vorhandene Einhängepunkte (`Services` in `crates/ui-egui/src/lib.rs`): `pick_open`, `pick_save`,
`can_restore`, `restore_file`, `browser_files`. Die Startseite (`crates/ui-egui/src/home.rs`) hat
die Beispiele fest einkompiliert und `MAX_RECENT = 12` Einträge für „Recent designs".

## Schnittstelle des Datei-Servers (umgesetzt in M1, `cloud/server/`)

| Aufruf | Zweck | Antworten |
|---|---|---|
| `GET /api/health` | Lebenszeichen, ohne Anmeldung | `200` |
| `GET /api/designs` | Liste: Name, Größe, `modified_ms`, ETag, Sperre (`owner`, `expires_in_s`, `mine`), dazu `max_bytes` | `200` |
| `GET /api/designs/{name}` | Datei laden, ETag im Header | `200`, `404` |
| `PUT /api/designs/{name}` | Speichern. Gibt es die Datei, ist `If-Match: <ETag>` Pflicht (`*` überschreibt ohne Prüfung) | `201` neu, `200`, `428` ohne If-Match, `412` ETag veraltet, `423` fremd gesperrt, `413` zu groß |
| `DELETE /api/designs/{name}` | Löschen (`If-Match` optional) | `204`, `404`, `412`, `423` |
| `POST /api/designs/{name}/lock` | Sperre nehmen, liefert `token` | `200`, `409` mit Besitzer |
| `PUT /api/designs/{name}/lock` | Heartbeat, Header `X-Lock-Token` | `200`, `409` mit `lost: true` |
| `DELETE /api/designs/{name}/lock` | Freigeben mit `X-Lock-Token`, oder `?force=1` für eine fremde Sperre | `204`, `409` |

- **Schreiben unter Sperre:** Ist ein Design gesperrt, geht `PUT` und `DELETE` nur mit dem Header
  `X-Lock-Token` der Sperre. Auch dieselbe Person in einem zweiten Tab bekommt `423`.
- **Namen:** Buchstaben, Ziffern, Leerzeichen und `- _ . ( ) ,`; Endung `.solidcraft`, `.solvecraft`
  oder `.json`; keine Pfade, kein führender Punkt, höchstens 200 Bytes. Der Name steht prozentkodiert
  im Pfad (`Mein%20Geh%C3%A4use.solidcraft`).
- **ETag** besteht aus Inode, Änderungszeit und Größe. Er ändert sich bei jedem Schreiben, auch bei
  einer Änderung von außen (z. B. durch Nextcloud).
- **Benutzer:** Der Name kommt im Header `X-Remote-User` vom Reverse Proxy (Apache) nach der
  Anmeldung, nie vom Client. Der Server vertraut ihm blind und lauscht deshalb nur lokal.

## Sperren

- Gesperrt wird **beim Öffnen**, nicht erst beim ersten Edit: Die App kennt keinen Nur-Lesen-Modus,
  und sonst scheitert der Zweite erst beim Speichern.
- Ablauf 60 s, Heartbeat alle 20 s. Beim Schließen des Tabs wird per `fetch(..., {keepalive: true})`
  freigegeben, nach einem Absturz läuft die Sperre von allein ab.
- Ist ein Design schon gesperrt, öffnet die App eine **lokale Kopie** und zeigt „gesperrt von X".
  Speichern ist dann nur unter neuem Namen möglich.
- Mehrere Tabs: pro offenem Server-Design eine eigene Sperre.
- Eine Sperre lässt sich von Hand aufheben (`DELETE …/lock?force=1`), z. B. wenn ein Notebook
  zugeklappt wurde. Das darf jede angemeldete Person: Bei drei vertrauten Leuten reicht das.
- Ein Heartbeat mit dem alten Token klappt auch nach kurzem Ausfall noch, solange niemand anderes
  die Sperre genommen hat (Notebook im Ruhezustand).
- Der Server hält Sperren im Speicher; bei einem Neustart sind alle frei.

## Updates

Upstream-Änderungen werden per Merge in diesen Fork geholt (Anleitung in `cloud/README.md`).
In M5 übernimmt das ein nächtlicher Workflow, der einen Pull Request öffnet und das Image zur
Probe baut. Schlägt das fehl, bleibt das alte Image aktiv.

Weil SolidCraft sein Dateiformat fast mit jeder Version erhöht (neue Dateien öffnen nicht in
älteren Versionen), sollen alle Nutzer nach einem Update neu laden. Der Service Worker der App
lädt neue Builds selbst nach und startet sie beim nächsten Aufruf.

## Offene Punkte

- Größenlimit beim Vorladen der Designs für die Startseite (SolidCraft erlaubt bis 256 MB pro
  Design).
- Anmeldung: zunächst Basic Auth in Apache, später evtl. Nextcloud-Konten über OIDC.
- Upstream plant eigenen Cloud-Speicher (`docs/web-runtime.md`). Vor größeren Schritten dort in
  den Issues nachsehen.
