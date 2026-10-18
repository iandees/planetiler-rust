import com.onthegomap.planetiler.geo.GeoUtils;
import com.onthegomap.planetiler.stats.Stats;
import java.io.BufferedReader;
import java.io.FileReader;
import java.io.PrintStream;
import java.util.ArrayList;
import java.util.List;
import java.util.Random;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.GeometryFactory;
import org.locationtech.jts.geom.LineString;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.geom.util.AffineTransformation;
import org.locationtech.jts.io.WKBReader;
import org.locationtech.jts.io.WKBWriter;
import org.locationtech.jts.precision.GeometryPrecisionReducer;

/**
 * Generates the expected outputs of {@code GeoUtils.snapAndFixPolygon} and {@code GeometryPrecisionReducer.reduce}
 * (tile precision, 1/16 px) for the Rust snap-rounding tests
 * ({@code crates/planetiler-core/tests/resources/snapround/*.txt.gz}, read by {@code polygon_overlay::tests}).
 * <p>
 * Inputs are deterministic synthetic geometries in tile pixel coordinates (near-touching and self-touching polygons,
 * slivers, spikes, holes near their shell, small jittered stars, crossing lines) and, optionally, real geometries
 * read from a file with one hex WKB per line (e.g. clipped tile polygons). Every synthetic input is valid, except the
 * {@code invalid-*} cases (which exercise the fix-then-reduce path) and the overlapping polygons of
 * {@code collection}. {@code tiles.txt.gz} is from a file of the inputs of Rust {@code snap_and_fix_polygon} calls
 * whose pointwise rounding was invalid (dumped by temporary instrumentation from OpenMapTiles Switzerland and Monaco
 * runs; a sample of the Switzerland ones), most of them invalid themselves. Output, one block per case:
 *
 * <pre>
 * CASE &lt;kind&gt; &lt;input hex WKB&gt;
 * SNAP &lt;hex WKB of snapAndFixPolygon, or ERROR&gt;
 * REDUCE &lt;hex WKB of GeometryPrecisionReducer.reduce, or ERROR&gt;
 * </pre>
 * <p>
 * Usage: {@code java -cp planetiler-dist-*-with-deps.jar bench/java/SnapRoundCases.java synthetic [count] | gzip
 * > synthetic.txt.gz} or {@code ... SnapRoundCases.java file inputs.hex | gzip > tiles.txt.gz}
 */
public class SnapRoundCases {

  private static final GeometryFactory GF = GeoUtils.JTS_FACTORY;
  private static final WKBWriter WRITER = new WKBWriter(2, false);

  public static void main(String[] args) throws Exception {
    PrintStream out = new PrintStream(new java.io.BufferedOutputStream(System.out), false);
    if (args[0].equals("synthetic")) {
      int count = args.length > 1 ? Integer.parseInt(args[1]) : 4000;
      Random rnd = new Random(42);
      for (int i = 0; i < count; i++) {
        String kind = KINDS[i % KINDS.length];
        Geometry g = generate(kind, rnd);
        if (g == null || g.isEmpty()) {
          continue;
        }
        if (!kind.startsWith("invalid") && !kind.startsWith("line") && !g.isValid()) {
          g = g.buffer(0);
          if (g.isEmpty() || !g.isValid()) {
            continue;
          }
        }
        emit(out, kind, g);
      }
    } else {
      WKBReader reader = new WKBReader(GF);
      try (BufferedReader in = new BufferedReader(new FileReader(args[1]))) {
        String line;
        while ((line = in.readLine()) != null) {
          if (!line.isBlank()) {
            emit(out, "tile", reader.read(WKBReader.hexToBytes(line.trim())));
          }
        }
      }
    }
    out.flush();
  }

  private static void emit(PrintStream out, String kind, Geometry g) {
    out.println("CASE " + kind + " " + WKBWriter.toHex(WRITER.write(g)));
    String snap;
    try {
      snap = hex(GeoUtils.snapAndFixPolygon(g, Stats.inMemory(), "test"));
    } catch (Exception e) {
      snap = "ERROR";
    }
    out.println("SNAP " + snap);
    String reduce;
    try {
      reduce = hex(GeometryPrecisionReducer.reduce(g, GeoUtils.TILE_PRECISION));
    } catch (Exception e) {
      reduce = "ERROR";
    }
    out.println("REDUCE " + reduce);
  }

  private static String hex(Geometry g) {
    return WKBWriter.toHex(WRITER.write(g));
  }

  private static final String[] KINDS = {
    "boxes", "star", "holes", "comb", "blob", "spike", "grid", "invalid-bowtie", "line", "lines", "collection"
  };

  /** A coordinate near a random grid line: an integer multiple of 1/16 px plus a small random offset. */
  private static double nearGrid(Random rnd, double max) {
    double v = Math.floor(rnd.nextDouble() * max * 16) / 16;
    double r = rnd.nextDouble();
    if (r < 0.3) {
      return v + (rnd.nextDouble() - 0.5) / 160; // within 1/10 grid unit
    } else if (r < 0.5) {
      return v + 1.0 / 32 + (rnd.nextDouble() - 0.5) / 1600; // near half a grid unit
    }
    return v + rnd.nextDouble() / 16;
  }

  private static Polygon ring(Coordinate... pts) {
    Coordinate[] closed = new Coordinate[pts.length + 1];
    System.arraycopy(pts, 0, closed, 0, pts.length);
    closed[pts.length] = pts[0].copy();
    return GF.createPolygon(closed);
  }

  private static Polygon rotatedRect(Random rnd, double cx, double cy, double w, double h) {
    Polygon p = ring(new Coordinate(-w / 2, -h / 2), new Coordinate(w / 2, -h / 2), new Coordinate(w / 2, h / 2),
      new Coordinate(-w / 2, h / 2));
    AffineTransformation t = AffineTransformation.rotationInstance(rnd.nextDouble() * Math.PI).translate(cx, cy);
    return (Polygon) t.transform(p);
  }

  private static Polygon star(Random rnd, double cx, double cy, double r, int n, double jitter) {
    Coordinate[] pts = new Coordinate[n];
    double phase = rnd.nextDouble() * Math.PI;
    for (int i = 0; i < n; i++) {
      double a = phase + 2 * Math.PI * i / n;
      double rr = r * (i % 2 == 0 ? 1 : 0.3 + 0.6 * rnd.nextDouble()) * (1 + jitter * (rnd.nextDouble() - 0.5));
      pts[i] = new Coordinate(cx + rr * Math.cos(a), cy + rr * Math.sin(a));
    }
    return ring(pts);
  }

  private static Geometry generate(String kind, Random rnd) {
    double scale = Math.pow(10, rnd.nextDouble() * 2 - 1.3); // ~0.05 .. 5 px features
    double cx = 10 + rnd.nextDouble() * 236, cy = 10 + rnd.nextDouble() * 236;
    switch (kind) {
      case "boxes": {
        // near-touching rotated rectangles: gaps of a fraction of a grid unit
        int n = 2 + rnd.nextInt(5);
        List<Polygon> polys = new ArrayList<>();
        double w = scale * (1 + rnd.nextDouble() * 4), h = scale * (0.05 + rnd.nextDouble());
        double angle = rnd.nextDouble() * Math.PI;
        for (int i = 0; i < n; i++) {
          double gap = Math.pow(10, -1 - rnd.nextDouble() * 3);
          Polygon p = ring(new Coordinate(0, i * (h + gap)), new Coordinate(w, i * (h + gap)),
            new Coordinate(w, i * (h + gap) + h), new Coordinate(0, i * (h + gap) + h));
          polys.add((Polygon) AffineTransformation.rotationInstance(angle).translate(cx, cy).transform(p));
        }
        return GF.createMultiPolygon(polys.toArray(Polygon[]::new));
      }
      case "star":
        return star(rnd, cx, cy, scale, 6 + 2 * rnd.nextInt(20), 0.3);
      case "holes": {
        double size = scale * 10;
        Polygon shell = rotatedRect(rnd, cx, cy, size, size * (0.5 + rnd.nextDouble()));
        List<LinearRing> holes = new ArrayList<>();
        int n = 1 + rnd.nextInt(6);
        Coordinate[] sc = shell.getExteriorRing().getCoordinates();
        for (int i = 0; i < n; i++) {
          // a small hole near (inside) a random point of the shell boundary
          int e = rnd.nextInt(4);
          double t = rnd.nextDouble();
          Coordinate a = sc[e], b = sc[e + 1];
          double px = a.x + (b.x - a.x) * t, py = a.y + (b.y - a.y) * t;
          double ix = cx - px, iy = cy - py, len = Math.hypot(ix, iy);
          double d = Math.pow(10, -1 - rnd.nextDouble() * 3) + (rnd.nextBoolean() ? 0 : 0.05);
          double hs = scale * (0.05 + rnd.nextDouble() * 0.5);
          Polygon hole = rotatedRect(rnd, px + ix / len * (d + hs), py + iy / len * (d + hs), hs, hs);
          holes.add(GF.createLinearRing(hole.getExteriorRing().getCoordinates()));
        }
        return GF.createPolygon(GF.createLinearRing(sc), holes.toArray(LinearRing[]::new));
      }
      case "comb": {
        // thin teeth separated by thin gaps
        int n = 2 + rnd.nextInt(8);
        double tooth = scale * 0.2 * rnd.nextDouble(), gap = Math.pow(10, -1 - rnd.nextDouble() * 2.5) * 0.3;
        double len = scale * (1 + 3 * rnd.nextDouble());
        List<Coordinate> pts = new ArrayList<>();
        pts.add(new Coordinate(0, 0));
        double x = 0;
        for (int i = 0; i < n; i++) {
          pts.add(new Coordinate(x, len));
          pts.add(new Coordinate(x + tooth, len));
          x += tooth;
          if (i < n - 1) {
            pts.add(new Coordinate(x, -0.0 + scale * 0.1));
            pts.add(new Coordinate(x + gap, scale * 0.1));
            x += gap;
          }
        }
        pts.add(new Coordinate(x, 0));
        Polygon p = ring(pts.toArray(Coordinate[]::new));
        return AffineTransformation.rotationInstance(rnd.nextDouble() * Math.PI).translate(cx, cy).transform(p);
      }
      case "blob": {
        // a polygon with vertices near the grid
        int n = 5 + rnd.nextInt(40);
        Coordinate[] pts = new Coordinate[n];
        double r = scale * 3;
        for (int i = 0; i < n; i++) {
          double a = 2 * Math.PI * i / n;
          double rr = r * (0.5 + 0.5 * rnd.nextDouble());
          pts[i] = new Coordinate(cx + rr * Math.cos(a), cy + rr * Math.sin(a));
          if (rnd.nextBoolean()) {
            pts[i] = new Coordinate(nearGrid(rnd, 0) + Math.floor(pts[i].x * 16) / 16,
              nearGrid(rnd, 0) + Math.floor(pts[i].y * 16) / 16);
          }
        }
        return ring(pts);
      }
      case "spike": {
        // a shell with thin spikes in and out
        int n = 4 + rnd.nextInt(12);
        List<Coordinate> pts = new ArrayList<>();
        double r = scale * 3;
        for (int i = 0; i < n; i++) {
          double a = 2 * Math.PI * i / n;
          pts.add(new Coordinate(cx + r * Math.cos(a), cy + r * Math.sin(a)));
          if (rnd.nextInt(3) == 0) {
            double a2 = a + Math.PI / n;
            double w = Math.pow(10, -2 - rnd.nextDouble() * 2);
            double rs = r * (rnd.nextBoolean() ? 0.3 : 1.6);
            pts.add(new Coordinate(cx + r * Math.cos(a2 - w), cy + r * Math.sin(a2 - w)));
            pts.add(new Coordinate(cx + rs * Math.cos(a2), cy + rs * Math.sin(a2)));
            pts.add(new Coordinate(cx + r * Math.cos(a2 + w), cy + r * Math.sin(a2 + w)));
          }
        }
        return ring(pts.toArray(Coordinate[]::new));
      }
      case "grid": {
        // squares with vertices close to pixel centers / half units, many touching
        List<Polygon> polys = new ArrayList<>();
        int n = 2 + rnd.nextInt(10);
        for (int i = 0; i < n; i++) {
          double x0 = Math.floor(cx * 16) / 16 + rnd.nextInt(8) / 16.0, y0 = Math.floor(cy * 16) / 16 + rnd.nextInt(8) / 16.0;
          double x1 = x0 + (1 + rnd.nextInt(4)) / 16.0, y1 = y0 + (1 + rnd.nextInt(4)) / 16.0;
          double j = 1.0 / 32;
          polys.add(ring(new Coordinate(x0 + (rnd.nextDouble() - 0.5) * j, y0 + (rnd.nextDouble() - 0.5) * j),
            new Coordinate(x1 + (rnd.nextDouble() - 0.5) * j, y0 + (rnd.nextDouble() - 0.5) * j),
            new Coordinate(x1 + (rnd.nextDouble() - 0.5) * j, y1 + (rnd.nextDouble() - 0.5) * j),
            new Coordinate(x0 + (rnd.nextDouble() - 0.5) * j, y1 + (rnd.nextDouble() - 0.5) * j)));
        }
        Geometry union = GF.createMultiPolygon(polys.toArray(Polygon[]::new)).buffer(0);
        return union;
      }
      case "invalid-bowtie": {
        double s = scale * 4;
        return ring(new Coordinate(cx, cy), new Coordinate(cx + s, cy + s * rnd.nextDouble()),
          new Coordinate(cx + s, cy), new Coordinate(cx, cy + s * rnd.nextDouble()));
      }
      case "line":
      case "lines": {
        int nl = kind.equals("line") ? 1 : 2 + rnd.nextInt(4);
        LineString[] lines = new LineString[nl];
        for (int l = 0; l < nl; l++) {
          int n = 2 + rnd.nextInt(10);
          Coordinate[] pts = new Coordinate[n];
          for (int i = 0; i < n; i++) {
            pts[i] = new Coordinate(cx + nearGrid(rnd, scale * 2), cy + nearGrid(rnd, scale * 2));
          }
          lines[l] = GF.createLineString(pts);
        }
        return nl == 1 ? lines[0] : GF.createMultiLineString(lines);
      }
      case "collection": {
        return GF.createGeometryCollection(new Geometry[]{
          star(rnd, cx, cy, scale, 8, 0.3),
          star(rnd, cx + scale * 0.5, cy, scale, 8, 0.3),
          GF.createLineString(new Coordinate[]{new Coordinate(cx, cy), new Coordinate(cx + nearGrid(rnd, 2), cy)}),
          GF.createPoint(new Coordinate(cx + rnd.nextDouble(), cy))
        });
      }
      default:
        throw new IllegalArgumentException(kind);
    }
  }
}
