import com.onthegomap.planetiler.VectorTile;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.CoordinateXY;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.GeometryFactory;

/**
 * Micro-benchmark for vector tile geometry + tile encoding, equivalent to the Rust
 * crates/planetiler-benchmarks/src/bin/benchmark_vector_tile.rs (identical deterministic workload).
 * <p>
 * Usage: {@code java -cp planetiler.jar bench/java/BenchmarkVectorTile.java [iterations]}
 */
public class BenchmarkVectorTile {

  private static final GeometryFactory GF = new GeometryFactory();
  private static final String[] KINDS =
    {"residential", "park", "water", "forest", "farm", "school", "retail", "industrial", "grass", "wood"};

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

  record Input(String layer, Geometry geom, Map<String, Object> tags) {}

  static List<Input> workload() {
    Rng rng = new Rng(42);
    List<Input> result = new ArrayList<>(3000);
    for (long i = 0; i < 1000; i++) {
      double cx = rng.nextDouble() * 256;
      double cy = rng.nextDouble() * 256;
      double r = 1 + rng.nextDouble() * 4;
      Coordinate[] coords = new Coordinate[9];
      for (int j = 0; j < 8; j++) {
        double a = Math.PI * 2 * j / 8;
        double rr = r * (0.75 + rng.nextDouble() * 0.5);
        coords[j] = new CoordinateXY(cx + rr * Math.cos(a), cy + rr * Math.sin(a));
      }
      coords[8] = coords[0].copy();
      Map<String, Object> tags = new LinkedHashMap<>();
      tags.put("kind", KINDS[(int) rng.nextInt(10)]);
      tags.put("rank", rng.nextInt(20));
      tags.put("area", Math.floor(rng.nextDouble() * 1000) / 10);
      tags.put("id", i);
      result.add(new Input("polygons", GF.createPolygon(coords), tags));
    }
    for (long i = 0; i < 1000; i++) {
      double x = rng.nextDouble() * 256;
      double y = rng.nextDouble() * 256;
      Coordinate[] coords = new Coordinate[10];
      for (int j = 0; j < 10; j++) {
        coords[j] = new CoordinateXY(x, y);
        x += rng.nextDouble() * 8 - 4;
        y += rng.nextDouble() * 8 - 4;
      }
      Map<String, Object> tags = new LinkedHashMap<>();
      tags.put("kind", KINDS[(int) rng.nextInt(10)]);
      tags.put("rank", rng.nextInt(20));
      tags.put("oneway", rng.nextInt(2) == 1);
      tags.put("id", i);
      result.add(new Input("lines", GF.createLineString(coords), tags));
    }
    for (long i = 0; i < 1000; i++) {
      double x = rng.nextDouble() * 256;
      double y = rng.nextDouble() * 256;
      Map<String, Object> tags = new LinkedHashMap<>();
      tags.put("kind", KINDS[(int) rng.nextInt(10)]);
      tags.put("rank", rng.nextInt(20));
      tags.put("name", "name" + i);
      result.add(new Input("points", GF.createPoint(new CoordinateXY(x, y)), tags));
    }
    return result;
  }

  static List<VectorTile.Feature> encodeFeatures(List<Input> input) {
    List<VectorTile.Feature> result = new ArrayList<>(input.size());
    for (int i = 0; i < input.size(); i++) {
      var in = input.get(i);
      result.add(new VectorTile.Feature(in.layer, i + 1, VectorTile.encodeGeometry(in.geom), in.tags));
    }
    return result;
  }

  static byte[] encodeTile(List<VectorTile.Feature> features) {
    return new VectorTile()
      .addLayerFeatures("polygons", features.subList(0, 1000))
      .addLayerFeatures("lines", features.subList(1000, 2000))
      .addLayerFeatures("points", features.subList(2000, 3000))
      .encode();
  }

  static volatile Object sink;

  public static void main(String[] args) {
    int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
    List<Input> input = workload();

    // warmup
    for (int i = 0; i < Math.max(1, iterations / 4); i++) {
      sink = encodeTile(encodeFeatures(input));
    }

    long start = System.nanoTime();
    for (int i = 0; i < iterations; i++) {
      List<VectorTile.VectorGeometry> geoms = new ArrayList<>(input.size());
      for (var in : input) {
        geoms.add(VectorTile.encodeGeometry(in.geom));
      }
      sink = geoms;
    }
    double geomSecs = (System.nanoTime() - start) / 1e9;
    List<VectorTile.Feature> features = encodeFeatures(input);

    start = System.nanoTime();
    int bytes = 0;
    for (int i = 0; i < iterations; i++) {
      byte[] encoded = encodeTile(features);
      sink = encoded;
      bytes = encoded.length;
    }
    double tileSecs = (System.nanoTime() - start) / 1e9;

    System.out.printf("java iterations=%d features/tile=%d%n", iterations, features.size());
    System.out.printf("encodeGeometry: %.0f features/sec (%.3f ms per 3000)%n",
      iterations * (double) input.size() / geomSecs, geomSecs * 1000 / iterations);
    System.out.printf("tile encode:    %.0f tiles/sec (%.3f ms per tile), %d bytes/tile%n",
      iterations / tileSecs, tileSecs * 1000 / iterations, bytes);
  }
}
