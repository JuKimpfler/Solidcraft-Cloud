# Datei-Server

Kleiner HTTP-Dienst, der die Designs des Teams als Dateien in einem Ordner hält: Liste, Laden,
Speichern mit ETag (kein stilles Überschreiben) und Sperren mit Ablauf. Rust, drei Abhängigkeiten
(`tiny_http`, `serde_json`, `getrandom`), eigenständiges Projekt außerhalb des SolidCraft-Workspaces.
Schnittstelle und Sperr-Konzept: [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

## Bauen und starten

```sh
cd cloud/server
cargo build --release          # Binärdatei: target/release/solidcraft-cloud-server
SOLIDCRAFT_CLOUD_DATA=/srv/solidcraft/designs ./target/release/solidcraft-cloud-server
```

| Variable | Bedeutung | Standard |
|---|---|---|
| `SOLIDCRAFT_CLOUD_DATA` | Ordner mit den Designs (wird angelegt) | **Pflicht** |
| `SOLIDCRAFT_CLOUD_BIND` | Adresse `ip:port` | `127.0.0.1:8081` |
| `SOLIDCRAFT_CLOUD_MAX_BYTES` | Größte erlaubte Datei | `268435456` (256 MiB, wie SolidCraft) |
| `SOLIDCRAFT_CLOUD_LOCK_TTL_SECS` | Sperre ohne Heartbeat gilt so lange (5 bis 3600) | `60` |

Der Server schreibt pro Anfrage eine Zeile ins Protokoll (Benutzer, Methode, Pfad, Status), nie
Inhalte oder Token.

## Sicherheit

- **Der Server vertraut dem Header `X-Remote-User` blind.** Er lauscht deshalb standardmäßig nur auf
  `127.0.0.1`, und nur der Reverse Proxy darf ihn erreichen. Bei einer anderen Adresse warnt er beim
  Start.
- Der Proxy muss den Header **immer selbst setzen und einen vom Client mitgeschickten verwerfen**
  (Beispiel unten). Nach dem Einrichten prüfen:
  `curl -u alice:PASSWORT -H 'X-Remote-User: bob' https://DEINE-DOMAIN/api/designs` muss die
  Sperren als „alice" zeigen (`mine`), nie als „bob".
- Dateinamen werden streng geprüft (keine Pfade, keine versteckten Dateien, nur `.solidcraft`,
  `.solvecraft`, `.json`). Der Server schreibt nur in seinen Datenordner.
- Der Server setzt keine Zeitlimits für langsame Clients. Das übernimmt Apache (`Timeout`,
  `mod_reqtimeout`).

## Einrichtung auf dem Raspberry Pi

**Nicht auf dem Pi getestet.** Die Schritte sind der übliche Weg, bitte einmal von Hand
durchgehen. Getestet ist der Server selbst (siehe unten), nicht seine Einbindung.

1. Rust auf dem Pi installieren (`rustup`) und den Server bauen. Das Projekt ist klein; auf einem
   Pi 5 sollte der Build in Minuten fertig sein. Alternativ baut der Workflow
   `cloud-server.yml` eine arm64-Binärdatei als Artefakt.
2. Benutzer und Ordner anlegen:
   ```sh
   sudo useradd --system --no-create-home --shell /usr/sbin/nologin solidcraft
   sudo install -d -o solidcraft -g www-data -m 2770 /srv/solidcraft/designs
   sudo install -m 755 target/release/solidcraft-cloud-server /usr/local/bin/
   ```
3. Dienst `/etc/systemd/system/solidcraft-cloud-server.service`:
   ```ini
   [Unit]
   Description=SolidCraft_Cloud Datei-Server
   After=network.target

   [Service]
   User=solidcraft
   Group=www-data
   UMask=0007
   Environment=SOLIDCRAFT_CLOUD_DATA=/srv/solidcraft/designs
   Environment=SOLIDCRAFT_CLOUD_BIND=127.0.0.1:8081
   ExecStart=/usr/local/bin/solidcraft-cloud-server
   Restart=on-failure
   NoNewPrivileges=true
   ProtectSystem=strict
   ProtectHome=true
   PrivateTmp=true
   ReadWritePaths=/srv/solidcraft/designs

   [Install]
   WantedBy=multi-user.target
   ```
   Dann `sudo systemctl enable --now solidcraft-cloud-server`.
4. Apache (Module `proxy`, `proxy_http`, `headers`, `rewrite`, `auth_basic`; `htpasswd -c
   /etc/apache2/solidcraft.htpasswd alice`). In den bestehenden HTTPS-VirtualHost der Subdomain:
   ```apache
   <Location />
       AuthType Basic
       AuthName "SolidCraft"
       AuthUserFile /etc/apache2/solidcraft.htpasswd
       Require valid-user
   </Location>

   # Den Benutzer vom Client nie übernehmen, nur den angemeldeten setzen.
   RequestHeader unset X-Remote-User
   RewriteEngine On
   RewriteCond %{LA-U:REMOTE_USER} (.+)
   RewriteRule .* - [E=PROXY_USER:%1,NS]
   RequestHeader set X-Remote-User "%{PROXY_USER}e" env=PROXY_USER

   LimitRequestBody 268435456
   ProxyTimeout 300
   ProxyPass        /api/ http://127.0.0.1:8081/api/
   ProxyPassReverse /api/ http://127.0.0.1:8081/api/
   ProxyPass        /     http://127.0.0.1:8080/
   ProxyPassReverse /     http://127.0.0.1:8080/
   ```
   (Port 8080 ist das Web-Image aus M0.)
5. **Nextcloud:** Den Ordner `/srv/solidcraft/designs` als „Externer Speicher" vom Typ „Lokal"
   einbinden (App „External storage support"). Der Server und Nextcloud (`www-data`) teilen sich die
   Gruppe `www-data`; `UMask=0007` und das Gruppen-Setgid-Bit sorgen dafür, dass beide die Dateien
   des anderen lesen und schreiben können. Änderungen, die der Server macht, sieht Nextcloud je
   nach Einstellung „Auf Änderungen prüfen" sofort oder nach `occ files:scan`. Das Verzeichnis
   `.tmp/` im Datenordner ist versteckt und nur für laufende Uploads gedacht.

## Schnell ausprobieren

```sh
B=http://127.0.0.1:8081/api
curl -X PUT  -H 'X-Remote-User: alice' --data-binary @Teil.solidcraft "$B/designs/Teil.solidcraft"   # 201
curl         -H 'X-Remote-User: bob'   "$B/designs"                                                     # Liste
curl -X POST -H 'X-Remote-User: alice' "$B/designs/Teil.solidcraft/lock"                                # Token
curl -X PUT  -H 'X-Remote-User: bob' -H 'If-Match: "…"' --data-binary x "$B/designs/Teil.solidcraft"  # 423
```

## Getestet

- `cargo test`: 30 Tests für Namensprüfung, ETag-Konflikte, atomares Speichern, Größenlimit,
  Sperren (Ablauf, Heartbeat, Übernahme, Token, Notbremse) und die HTTP-Schnittstelle inklusive
  Pfad-Tricks (`..%2F`, `%00`, ungültiges UTF-8). `cargo clippy -- -D warnings` und
  `cargo fmt --check` sind sauber; kein `unwrap`/`expect`/`panic` im Produktivcode.
- Von Hand gegen den laufenden Server mit `curl` (Linux, x86_64): vollständiger Ablauf mit zwei
  Benutzern und Sperre, Chunked-Upload ohne `Content-Length`, 150-MB-Datei hoch und herunter
  (Prüfsumme identisch, 0,4 s über Loopback, **Spitzen-RSS des Servers 3 MB**, weil gestreamt wird),
  Liste und kleine Speichervorgänge während des großen Uploads ohne Wartezeit.
- **Nicht getestet:** Betrieb auf dem Pi (arm64), Apache-Einbindung, Nextcloud-Einbindung,
  Zusammenspiel mit der App (kommt mit M2).

## Bekannte Grenzen

- Sperren liegen im Arbeitsspeicher: Nach einem Neustart des Dienstes sind alle frei.
- Alle Schreibzugriffe laufen kurz hintereinander durch ein gemeinsames Schloss (die Übertragung
  selbst nicht). Für drei Personen ist das unproblematisch.
- Die Notbremse `?force=1` darf jede angemeldete Person.
- Nur Linux/Unix (der ETag nutzt die Inode-Nummer).
