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

## Schnittstelle des Datei-Servers (Entwurf, M1)

| Aufruf | Zweck |
|---|---|
| `GET /api/designs` | Liste: Name, Größe, Änderungszeit, ETag, Sperre (wer, bis wann) |
| `GET /api/designs/{name}` | Datei laden, ETag im Header |
| `PUT /api/designs/{name}` | Speichern, `If-Match: <ETag>` Pflicht, bei Abweichung `412` |
| `DELETE /api/designs/{name}` | Löschen (nur ohne fremde Sperre) |
| `POST /api/designs/{name}/lock` | Sperre nehmen; `409` mit Besitzer, wenn schon gesperrt |
| `PUT /api/designs/{name}/lock` | Heartbeat, verlängert die Sperre |
| `DELETE /api/designs/{name}/lock` | Sperre freigeben |

Der Benutzername kommt aus der Anmeldung am Apache (`X-Remote-User`, vom Proxy gesetzt, nie vom
Client; der Server muss ihn vor Clients abschirmen, also nur auf dem lokalen Interface lauschen).
Dateinamen werden streng geprüft (keine Pfade, keine versteckten Dateien, Größenlimit).

## Sperren

- Gesperrt wird **beim Öffnen**, nicht erst beim ersten Edit: Die App kennt keinen Nur-Lesen-Modus,
  und sonst scheitert der Zweite erst beim Speichern.
- Ablauf 60 s, Heartbeat alle 20 s. Beim Schließen des Tabs wird per `fetch(..., {keepalive: true})`
  freigegeben, nach einem Absturz läuft die Sperre von allein ab.
- Ist ein Design schon gesperrt, öffnet die App eine **lokale Kopie** und zeigt „gesperrt von X".
  Speichern ist dann nur unter neuem Namen möglich.
- Mehrere Tabs: pro offenem Server-Design eine eigene Sperre.
- Eine Sperre lässt sich serverseitig von Hand aufheben (z. B. wenn ein Notebook zugeklappt wurde).
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
