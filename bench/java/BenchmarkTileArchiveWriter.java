import com.onthegomap.planetiler.FeatureCollector;
import com.onthegomap.planetiler.Profile;
import com.onthegomap.planetiler.archive.TileArchiveMetadata;
import com.onthegomap.planetiler.archive.TileArchiveWriter;
import com.onthegomap.planetiler.collection.FeatureGroup;
import com.onthegomap.planetiler.config.Arguments;
import com.onthegomap.planetiler.config.PlanetilerConfig;
import com.onthegomap.planetiler.geo.TileOrder;
import com.onthegomap.planetiler.mbtiles.Mbtiles;
import com.onthegomap.planetiler.reader.SimpleFeature;
import com.onthegomap.planetiler.reader.SourceFeature;
import com.onthegomap.planetiler.render.FeatureRenderer;
import com.onthegomap.planetiler.stats.ProcessInfo;
import com.onthegomap.planetiler.stats.Stats;
import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.ResultSet;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.GeometryFactory;

/**
 * Benchmark for the archive (tile encoding + writing) stage, equivalent to the Rust
 * crates/planetiler-benchmarks/src/bin/benchmark_tile_archive_writer.rs (identical deterministic workload).
 * <p>
 * Usage:
 * {@code java -cp planetiler.jar bench/java/BenchmarkTileArchiveWriter.java [features] [rounds] [tmpdir]}
 */
public class BenchmarkTileArchiveWriter {

  private static final GeometryFactory GF = new GeometryFactory();
  private static final String[] KINDS =
    {"residential", "park", "water", "forest", "farm", "school", "retail", "industrial", "grass", "wood"};
  private static final double MIN_LON = 5.9, MIN_LAT = 45.8, WIDTH = 4.6, HEIGHT = 2.0;

  /** Deterministic 64-bit LCG (same constants and arithmetic as the Rust version). */
  static final class Rng {
    long state;

    Rng(long seed) {
      state = seed;
    }

    double nextDouble() {
      state = state * 6364136223846793005L + 1442695040888963407L;
      return (state >>> 11) * (1.0 / (1L << 53));
    }

    long nextInt(long bound) {
      return (long) (nextDouble() * bound);
    }
  }

  static Map<String, Object> tags(long t, long minzoom, Rng rng) {
    Map<String, Object> tags = new LinkedHashMap<>();
    tags.put("t", t);
    tags.put("minzoom", minzoom);
    tags.put("kind", KINDS[(int) rng.nextInt(10)]);
    tags.put("rank", rng.nextInt(20));
    return tags;
  }

  static List<SimpleFeature> workload(int n) {
    Rng rng = new Rng(42);
    List<SimpleFeature> result = new ArrayList<>(n + 1);
    var water = GF.createPolygon(new Coordinate[]{
      new Coordinate(MIN_LON, MIN_LAT),
      new Coordinate(MIN_LON + WIDTH, MIN_LAT),
      new Coordinate(MIN_LON + WIDTH, MIN_LAT + HEIGHT),
      new Coordinate(MIN_LON, MIN_LAT + HEIGHT),
      new Coordinate(MIN_LON, MIN_LAT)
    });
    result.add(SimpleFeature.create(water, tags(0, 0, rng), 1));
    for (int i = 0; i < n; i++) {
      int u = i % 10;
      long id = i + 2L;
      if (u < 5) {
        var p = GF.createPoint(new Coordinate(MIN_LON + rng.nextDouble() * WIDTH, MIN_LAT + rng.nextDouble() * HEIGHT));
        long minzoom = 10 + rng.nextInt(5);
        var tags = tags(1, minzoom, rng);
        tags.put("name", "poi " + (i % 1000));
        result.add(SimpleFeature.create(p, tags, id));
      } else if (u < 8) {
        double x = MIN_LON + rng.nextDouble() * WIDTH;
        double y = MIN_LAT + rng.nextDouble() * HEIGHT;
        Coordinate[] coords = new Coordinate[6];
        for (int j = 0; j < 6; j++) {
          coords[j] = new Coordinate(x, y);
          x += (rng.nextDouble() * 2 - 1) * 0.002;
          y += (rng.nextDouble() * 2 - 1) * 0.002;
        }
        long minzoom = 6 + rng.nextInt(9);
        result.add(SimpleFeature.create(GF.createLineString(coords), tags(2, minzoom, rng), id));
      } else {
        double cx = MIN_LON + rng.nextDouble() * WIDTH;
        double cy = MIN_LAT + rng.nextDouble() * HEIGHT;
        double r = 0.0002 + rng.nextDouble() * 0.0018;
        Coordinate[] coords = new Coordinate[9];
        for (int j = 0; j < 8; j++) {
          double a = Math.PI * 2 * j / 8;
          double rr = r * (0.75 + rng.nextDouble() * 0.5);
          coords[j] = new Coordinate(cx + rr * Math.cos(a), cy + rr * Math.sin(a));
        }
        coords[8] = coords[0];
        long minzoom = 8 + rng.nextInt(7);
        result.add(SimpleFeature.create(GF.createPolygon(coords), tags(3, minzoom, rng), id));
      }
    }
    return result;
  }

  static class BenchProfile implements Profile {
    @Override
    public void processFeature(SourceFeature source, FeatureCollector features) {
      long t = source.getLong("t");
      int minzoom = (int) source.getLong("minzoom");
      var feature = switch ((int) t) {
        case 0 -> features.polygon("water");
        case 1 -> features.point("poi");
        case 2 -> features.line("road");
        default -> features.polygon("building");
      };
      feature.setZoomRange(minzoom, 14).inheritAttrFromSource("kind").inheritAttrFromSource("rank");
      if (t == 1) {
        feature.inheritAttrFromSource("name");
      }
    }

    @Override
    public String name() {
      return "bench";
    }
  }

  static double cpu() {
    return ProcessInfo.getProcessCpuTime().orElse(Duration.ZERO).toNanos() / 1e9;
  }

  public static void main(String[] args) throws Exception {
    int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
    int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
    Path tmp = Files.createTempDirectory(args.length > 2 ? Path.of(args[2]) : Path.of(System.getProperty("java.io.tmpdir")),
      "bench");
    var config = PlanetilerConfig.from(Arguments.of(Map.of("tmpdir", tmp.toString())));
    var stats = Stats.inMemory();
    var profile = new BenchProfile();

    long start = System.nanoTime();
    var features = workload(n);
    System.out.printf("generate: %d features in %.2fs%n", features.size(), (System.nanoTime() - start) / 1e9);

    var group = FeatureGroup.newDiskBackedFeatureGroup(TileOrder.TMS, tmp.resolve("feature.db"), profile, config,
      stats);
    start = System.nanoTime();
    double cpuStart = cpu();
    int threads = config.threads();
    int chunk = (features.size() + threads - 1) / threads;
    List<Thread> workers = new ArrayList<>();
    for (int t = 0; t < threads; t++) {
      var part = features.subList(Math.min(features.size(), t * chunk), Math.min(features.size(), (t + 1) * chunk));
      var thread = new Thread(() -> {
        var factory = new FeatureCollector.Factory(config, stats);
        var encoder = group.newRenderedFeatureEncoder();
        try (var writer = group.writerForThread()) {
          var renderer = new FeatureRenderer(config, rendered -> writer.accept(encoder.apply(rendered)), stats);
          for (var source : part) {
            var collector = factory.get(source);
            profile.processFeature(source, collector);
            for (var feature : collector) {
              renderer.accept(feature);
            }
          }
        } catch (Exception e) {
          throw new RuntimeException(e);
        }
      });
      thread.start();
      workers.add(thread);
    }
    for (var thread : workers) {
      thread.join();
    }
    features = null;
    System.out.printf("render: %d rendered features in %.2fs wall %.2fs cpu%n", group.numFeaturesWritten(),
      (System.nanoTime() - start) / 1e9, cpu() - cpuStart);
    start = System.nanoTime();
    cpuStart = cpu();
    group.prepare();
    System.out.printf("sort: %.2fs wall %.2fs cpu%n", (System.nanoTime() - start) / 1e9, cpu() - cpuStart);

    var metadata = new TileArchiveMetadata(profile, config);
    for (int round = 0; round < rounds; round++) {
      Path path = tmp.resolve("output" + round + ".mbtiles");
      start = System.nanoTime();
      cpuStart = cpu();
      try (var archive = Mbtiles.newWriteToFileDatabase(path, config.arguments())) {
        TileArchiveWriter.writeOutput(group, archive, () -> path.toFile().length(), metadata, null, config, stats);
      }
      double wall = (System.nanoTime() - start) / 1e9;
      double cpuTime = cpu() - cpuStart;
      long tiles;
      try (var db = Mbtiles.newReadOnlyDatabase(path);
        var statement = db.connection().createStatement();
        ResultSet rs = statement.executeQuery("select count(*) from tiles")) {
        rs.next();
        tiles = rs.getLong(1);
      }
      System.out.printf("archive round %d: %.2fs wall %.2fs cpu, %d tiles, %d bytes%n", round, wall, cpuTime, tiles,
        Files.size(path));
      Files.delete(path);
    }
  }
}
