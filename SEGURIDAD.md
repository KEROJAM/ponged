# Seguridad — escaneo OWASP ZAP del gateway HTTP

Pruebas de seguridad de la aplicación (XSS, inyección SQL, headers, etc.)
sobre la **página de moderación HTTP** del gateway (`src/bin/gateway.rs`),
usando **OWASP ZAP** en CI.

## Qué se escanea

El job [`security.yml`](.github/workflows/security.yml):

1. Compila el binario `ponged-gateway`.
2. Crea un moderador de prueba (`zap`, rol `admin`) en una SQLite efímera.
3. Arranca el gateway con `--http 8080` (escuchando en 127.0.0.1).
4. Corre el **baseline** de OWASP ZAP (`zap-baseline.py`) contra
   `http://127.0.0.1:8080/` desde el contenedor `ghcr.io/zaproxy/zaproxy`,
   cifrado de red del host.
5. Publica el informe (`report.html` + `report.json`) como artefacto
   **`informe-owasp-zap`** de la ejecución.
6. **Falla el job solo si aparece una alerta de riesgo High** (XSS, SQLi, …).
   Avisos Low/Medium se publican sin romper el pipeline (se revisan igualmente
   en el informe).

Se ejecuta en `push` a `main` cuando cambia `src/bin/gateway.rs` / `Cargo.toml`
y manualmente (`Actions → Seguridad (OWASP ZAP) → Run workflow`).

## Reproducción local

```bash
# 1) Cuenta de prueba
echo "pass" | ./target/debug/ponged-gateway --db /tmp/gw.sqlite \
  --add-moderator zap --role admin

# 2) Gateway con HTTP en 8080 (en segundo plano)
./target/debug/ponged-gateway --db /tmp/gw.sqlite --key /tmp/gw.key \
  --listen /ip4/127.0.0.1/tcp/40501 --http 8080 &

# 3) Escaneo (requiere Docker)
mkdir -p /tmp/zap
docker run --rm --network host -v /tmp/zap:/zap/wrk:rw \
  ghcr.io/zaproxy/zaproxy:stable \
  zap-baseline.py -t http://127.0.0.1:8080 \
    -J /zap/wrk/report.json -r /zap/wrk/report.html

# 4) Revisar: abrir /tmp/zap/report.html (o parsear report.json)
```

## Resultados

| Ejecución | Fecha | Alertas High | Alertas tot. | Decisión |
|-----------|-------|:------------:|:------------:|----------|
| pendiente (ver CI) | — | — | — | — |

Cómo leer el resultado del CI:

- **Job verde** = sin alertas de riesgo High; el XSS/SQLi está bajo umbral.
- **Job rojo** = el escaneo encontró al menos una alerta High; revisar el
  artefacto `informe-owasp-zap` y corregir antes de cerrar.
- El escáner sin sesión solo llega a la superficie pública: `/login`
  (formulario POST) y los `401/403` de la API; los endpoints autenticados
  (`/api/matches`, `/api/moderators*`) se cubren con las mitigaciones de
  abajo y requieren un scan autenticado si se quiere atacar la superficie.

## Mitigaciones ya implementadas (por diseño)

El gateway nace con mitigaciones que el escaneo comprueba:

- **SQLi**: todas las consultas son **parámetros enlazados** de rusqlite
  (`params![...]`); no se interpolan cadenas del cliente en SQL.
- **XSS**: todos los valores del monitor se escapan en la página
  (`esc()` en `MONITOR_HTML`); los mensajes del servidor vuelven como JSON y
  se insertan vía `textContent`/`esc`.
- **Autenticación**: contraseñas con **SHA-256 estirada (60 000 iteraciones) +
  sal de 16 B**, verificación en tiempo constante; sesiones por cookie
  `HttpOnly; SameSite=Strict` con token aleatorio de 32 B y TTL deslizante.
- **Rol admin/user**: gestión de cuentas solo con rol `admin`; un `user`
  recibe `403` en `/api/moderators*`.

## Referencias

- [SONARQUBE.md](SONARQUBE.md) — métricas de calidad/seguridad del repo.
- `.github/workflows/security.yml` — definición del escaneo.