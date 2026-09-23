# SonarQube / SonarCloud

Análisis estático de calidad y seguridad del código **Rust** con el analyzer
oficial de Sonar (soporta Rust desde 2025): métricas de complejidad,
duplicación, code smells, bugs y security hotspots; importa cobertura **LCOV**
y los lints de **Clippy** como reglas externas.

## Cómo se dispara

Workflow [`.github/workflows/sonar.yml`](.github/workflows/sonar.yml):

1. Compila los tests y genera la cobertura con **cargo-llvm-cov** → `target/lcov.info`.
2. Genera el reporte de **Clippy** → `target/clippy.json`.
3. Sube ambos como artefacto (`reportes-sonarqube`) — entregable del repo.
4. Ejecuta el **SonarQube Scanner** (importa LCOV + Clippy; no recompila).
5. El **Quality Gate** lo evalúa la integración de SonarCloud en GitHub
   (check "SonarCloud Code Analysis"), no se fuerza con una action aparte.

Se ejecuta en `push` a `main`, en cada PR y manualmente
(`Actions → SonarQube → Run workflow`).

> El shell **ligero** de desarrollo (`nix develop`) NO incluye LLVM ni
> cargo-llvm-cov (son pesados). Solo el shell `.#coverage`, usado por el CI,
> los trae.

## Configuración inicial (una sola vez)

1. **Crear el proyecto** en [SonarQube Cloud](https://sonarcloud.io) (o una
   instancia SonarQube Server). Copia el *project key*.
2. En `sonar-project.properties` deja el `sonar.projectKey` real y, si usas
   Cloud, descomenta y completa `sonar.organization`.
3. **Genera un token** (Account → Security) y guárdalo en GitHub:
   `Settings → Secrets and variables → Actions → Secrets` como `SONAR_TOKEN`.
4. Si usas **SonarQube Server propio**, crea también la *variable* de repo
   `SONAR_HOST_URL` con tu URL. Si no existe, el job asume
   `https://sonarcloud.io`.
5. Sube el workflow y corre el análisis.

## Uso local (opcional)

```bash
# Cobertura + resumen (requiere el shell coverage, pocos minutos)
nix develop .#coverage -c cargo llvm-cov --lcov --output-path target/lcov.info
nix develop .#coverage -c cargo llvm-cov report --summary-only

# Reporte Clippy (JSON para Sonar)
nix develop .#coverage -c cargo clippy --message-format=json > target/clippy.json
```

## Plantilla de métricas (para el informe de cierre)

Valores **medidos en este repositorio** (2026-09, tras los roles admin/user, el
hole-punching y 59 tests unitarios) para lo que se puede leer localmente; los
que solo expone el análisis de SonarCloud quedan marcados como *pendiente de la
primera ejecución* (necesita `SONAR_TOKEN` y el proyecto creado, ver arriba):

| Métrica                        | Valor obtenido |
|--------------------------------|---------------:|
| Líneas de código (NCLOC)       | ≈ 8 500 (`src/**/*.rs`, sin comentarios ni vacías) |
| Bugs                           | *pendiente (SonarCloud)*   |
| Vulnerabilidades               | *pendiente (SonarCloud)*   |
| Security hotspots              | *pendiente (SonarCloud)*; 0 High en el escaneo OWASP ZAP |
| Code smells                    | *pendiente (SonarCloud)*; proxy local: ~41 avisos de Clippy (ninguno nuevo al añadir roles) |
| Deuda técnica (minutos/días)   | *pendiente (SonarCloud)*   |
| Duplicación (%)                | *pendiente (SonarCloud)*   |
| Complejidad ciclomática        | *pendiente (SonarCloud)*; Clippy p. ej. señala 12 `type_complexity` |
| Cobertura de líneas (%)        | **21.98 %** global con `--all-targets` (ver tabla abajo) |
| Calificación global (A–F)      | *pendiente (SonarCloud)*   |
| Calificación de seguridad      | *pendiente (SonarCloud)*   |
| Quality Gate (pass/fail)       | *pendiente (SonarCloud)*   |

Cobertura por archivo (medida con `cargo llvm-cov --all-targets`):

| Archivo           | Líneas cubiertas |
|-------------------|-----------------:|
| `src/protocol.rs` | 100 %|
| `src/bin/gateway.rs` | 51.7 % (23 tests del gateway) |
| `src/sim.rs`      | 54.7 %|
| `src/config.rs`   | 28.0 %|
| `src/networking.rs` | 9.9 %|
| `src/history.rs`  | 0 % (sin tests) |
| `src/main.rs` / `src/menu.rs` | 0 % (UI Bevy, sin tests) |

> **Nota de cobertura**: el job `sonar.yml` genera el LCOV con
> `cargo llvm-cov --lcov` **sin `--all-targets`**, así que SonarCloud solo
> agrega los tests de la librería (protocol/sim/config) y verá ~11 % — la
> medición correcta (incluye los 23 tests del binario del gateway) es ~22 %.
> Para reproducir ambas, ver los comandos de abajo.

## Mediciones reproducibles

```bash
# Cobertura completa (lib + bins) — la correcta para el informe:
nix develop .#coverage -c cargo llvm-cov --all-targets --lcov --output-path target/lcov.info
nix develop .#coverage -c cargo llvm-cov report --summary-only    # ~22 %

# La que ve SonarCloud hoy (solo librería):
nix develop .#coverage -c cargo llvm-cov --lcov --output-path target/lcov.info

# Tests unitarios (recuento): 59 en total
nix develop -c cargo test
```

La cobertura también se puede leer sin Sonar con:

```bash
nix develop .#coverage -c cargo llvm-cov report --summary-only
```

## Notas

- El analyzer de Rust **no clona de la web**: toda regla llega vía
  **Clippy** (de ahí `sonar.rust.clippy.enabled=false` +
  `sonar.rust.clippyReport.reportPaths=target/clippy.json` en
  `sonar-project.properties`).
- Otros entregables de calidad: [TESTS.md](TESTS.md) (tests unitarios) y el
  pipeline [test.yml](.github/workflows/test.yml).
- Seguridad: [SEGURIDAD.md](SEGURIDAD.md) documenta el escaneo OWASP ZAP
  ([security.yml](.github/workflows/security.yml)) y las mitigaciones
  manuales ya presentes (SQL parametrizado, salida escapada, hashes estirados).