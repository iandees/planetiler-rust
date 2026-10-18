import com.onthegomap.planetiler.FeatureCollector;
import com.onthegomap.planetiler.Profile;
import com.onthegomap.planetiler.collection.FeatureGroup;
import com.onthegomap.planetiler.collection.LongLongMap;
import com.onthegomap.planetiler.collection.LongLongMultimap;
import com.onthegomap.planetiler.config.Arguments;
import com.onthegomap.planetiler.config.PlanetilerConfig;
import com.onthegomap.planetiler.geo.TileOrder;
import com.onthegomap.planetiler.reader.SourceFeature;
import com.onthegomap.planetiler.reader.osm.OsmInputFile;
import com.onthegomap.planetiler.reader.osm.OsmReader;
import com.onthegomap.planetiler.stats.Stats;
import com.onthegomap.planetiler.stats.Timer;
import com.onthegomap.planetiler.util.FileUtils;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Runs OsmReader pass1 + pass2 over an .osm.pbf file and reports wall/CPU time per pass. Equivalent to the Rust
 * crates/planetiler-benchmarks/src/bin/benchmark_osm_reader.rs (same profiles, same storage defaults).
 * <p>
 * Usage:
 * {@code java -cp planetiler.jar bench/java/BenchmarkOsmReader.java --osm_path=... --profile=null|emit [--iterations=N]}
 */
public class BenchmarkOsmReader {

  /** Emits one z14 feature per tagged element. */
  static class EmitProfile implements Profile {
    @Override
    public void processFeature(SourceFeature sourceFeature, FeatureCollector features) {
      if (sourceFeature.tags().isEmpty()) {
        return;
      }
      if (sourceFeature.isPoint()) {
        features.point("point").setMinZoom(14);
      } else if (sourceFeature.canBePolygon()) {
        features.polygon("polygon").setMinZoom(14);
      } else if (sourceFeature.canBeLine()) {
        features.line("line").setMinZoom(14);
      }
    }

    @Override
    public String name() {
      return "emit";
    }
  }

  public static void main(String[] args) throws Exception {
    var arguments = Arguments.fromArgs(args);
    String path = arguments.getString("osm_path", "osm file", "../planetiler-bench/switzerland-latest.osm.pbf");
    String profileName = arguments.getString("profile", "profile", "null");
    int iterations = arguments.getInteger("iterations", "iterations", 1);
    Path tmp = Path.of(arguments.getString("tmpdir", "tmp", "data/tmp/benchmark_osm_reader_java"));
    var config = PlanetilerConfig.from(arguments);
    System.err.println("osm_path=" + path + " profile=" + profileName + " threads=" + config.threads() +
      " process_threads=" + config.featureProcessThreads() + " write_threads=" + config.featureWriteThreads() +
      " nodemap=" + config.nodeMapType() + "/" + config.nodeMapStorage() + " lazy=" + config.osmLazyReads());
    Profile profile = switch (profileName) {
      case "null" -> new Profile.NullProfile();
      case "emit" -> new EmitProfile();
      default -> throw new IllegalArgumentException(profileName);
    };
    for (int i = 0; i < iterations; i++) {
      FileUtils.delete(tmp);
      Files.createDirectories(tmp);
      var stats = Stats.inMemory();
      var input = new OsmInputFile(Path.of(path), config.osmLazyReads());
      var nodeLocations = LongLongMap.from(config.nodeMapType(), config.nodeMapStorage(), tmp.resolve("node.db"),
        config.nodeMapMadvise());
      var multipolygons = LongLongMultimap.newReplaceableMultimap(config.multipolygonGeometryStorage(),
        tmp.resolve("multipolygon.db"), config.multipolygonGeometryMadvise());
      var group = FeatureGroup.newDiskBackedFeatureGroup(TileOrder.TMS, tmp.resolve("feature.db"), profile, config,
        stats);
      var reader = new OsmReader("osm", input, nodeLocations, multipolygons, profile, stats);
      var timer = Timer.start();
      reader.pass1(config);
      var pass1 = timer.stop().elapsed();
      timer = Timer.start();
      reader.pass2(group, config);
      var pass2 = timer.stop().elapsed();
      reader.close();
      System.err.printf("RESULT pass1: %s (%.3fs wall %.3fs cpu) pass2: %s (%.3fs wall %.3fs cpu) features=%d data_errors=%s%n",
        pass1, pass1.wall().toNanos() / 1e9, pass1.cpu().orElseThrow().toNanos() / 1e9,
        pass2, pass2.wall().toNanos() / 1e9, pass2.cpu().orElseThrow().toNanos() / 1e9,
        group.numFeaturesWritten(), new java.util.TreeMap<>(stats.dataErrors()));
      FileUtils.delete(tmp);
    }
  }
}
