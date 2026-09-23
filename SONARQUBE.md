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

Completar con los valores que muestra el proyecto en SonarQube
(**Measures / Quality Gate**) tras la primera ejecución:

| Métrica                        | Valor obtenido |
|--------------------------------|---------------:|
| Líneas de código (NCLOC)       | `________`     |
| Bugs                           | `________`     |
| Vulnerabilidades               | `________`     |
| Security hotspots              | `________`     |
| Code smells                    | `________`     |
| Deuda técnica (minutos/días)   | `________`     |
| Duplicación (%)                | `________`     |
| Complejidad ciclomática        | `________`     |
| Cobertura de líneas (%)        | `________`     |
| Calificación global (A–F)      | `________`     |
| Calificación de seguridad      | `________`     |
| Quality Gate (pass/fail)       | `________`     |

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