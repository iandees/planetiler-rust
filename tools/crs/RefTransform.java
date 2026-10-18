import com.onthegomap.planetiler.geo.GeoUtils;
import java.nio.file.Files;
import java.nio.file.Path;
import org.geotools.referencing.CRS;

/**
 * Computes reference coordinates with GeoTools the way planetiler does it, for the tests of
 * {@code crates/planetiler-core/src/geo/crs.rs}.
 *
 * <p>
 * Input (first argument), one case per line: {@code name<TAB>definition<TAB>x,y;x,y;...[<TAB>rust definition]} where
 * definition is anything {@code GeoUtils.decodeCRS} accepts or {@code @file.prj} (relative to the directory given as
 * second argument), and the optional rust definition is what the Rust test decodes instead (for inputs GeoTools does
 * not support, with an equivalent definition for GeoTools). Output (stdout): {@code name<TAB>rust definition<TAB>
 * points<TAB>lon,lat;...} with the transform of each point to {@code EPSG:4326} (longitude first, lenient, like
 * {@code ShapefileReader}), or {@code ERR message}.
 *
 * <pre>
 * tools/crs/java.sh RefTransform crates/planetiler-core/tests/resources/crs/cases.tsv crates/planetiler-core/tests/resources/crs
 * </pre>
 */
public class RefTransform {
  public static void main(String[] args) throws Exception {
    var wgs = CRS.decode("EPSG:4326", true);
    Path dir = Path.of(args.length > 1 ? args[1] : ".");
    for (String line : Files.readAllLines(Path.of(args[0]))) {
      if (line.isBlank() || line.startsWith("#")) {
        continue;
      }
      String[] parts = line.split("\t");
      String name = parts[0];
      String def = parts[1];
      String points = parts[2];
      String rustDef = parts.length > 3 ? parts[3] : def;
      StringBuilder sb = new StringBuilder();
      try {
        var crs = def.startsWith("@") ? CRS.parseWKT(Files.readString(dir.resolve(def.substring(1)))) :
          GeoUtils.decodeCRS(def);
        var xform = CRS.findMathTransform(crs, wgs, true);
        for (String pt : points.split(";")) {
          String[] xy = pt.split(",");
          double[] in = {Double.parseDouble(xy[0]), Double.parseDouble(xy[1])};
          double[] out = new double[2];
          xform.transform(in, 0, out, 0, 1);
          if (!sb.isEmpty()) {
            sb.append(';');
          }
          sb.append(out[0]).append(',').append(out[1]);
        }
      } catch (Exception e) {
        sb.setLength(0);
        sb.append("ERR ").append(String.valueOf(e).replace('\t', ' ').replace('\n', ' '));
      }
      System.out.println(name + "\t" + rustDef + "\t" + points + "\t" + sb);
    }
  }
}
