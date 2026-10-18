import java.io.FileOutputStream;
import java.io.PrintStream;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;
import org.geotools.api.metadata.citation.Citation;
import org.geotools.api.parameter.ParameterValue;
import org.geotools.api.referencing.crs.CoordinateReferenceSystem;
import org.geotools.api.referencing.crs.GeographicCRS;
import org.geotools.api.referencing.crs.ProjectedCRS;
import org.geotools.api.referencing.operation.MathTransform;
import org.geotools.referencing.operation.LinearTransform;
import org.geotools.metadata.iso.citation.Citations;
import org.geotools.referencing.CRS;
import org.geotools.referencing.operation.transform.AbstractMathTransform;
import org.geotools.referencing.operation.transform.ConcatenatedTransform;
import org.geotools.referencing.wkt.Formattable;

/**
 * Dumps every 2D geographic/projected CRS in the GeoTools EPSG database (gt-epsg-hsql) as one TSV line:
 *
 * <pre>
 * code, kind (G/P), CRS.getAxisOrder, axis directions, operation name to WGS84, samples, longitude-first WKT, datum
 * </pre>
 *
 * where samples are {@code lon,lat,x,y,lon2,lat2;...}: x,y = WGS84 -> CRS (longitude first, lenient) of a point
 * lon,lat in the area of use, lon2,lat2 = the inverse transform of x,y; and datum describes the datum shift GeoTools
 * uses from the geographic (base) CRS to WGS84: {@code helmert:dx,dy,dz,rx,ry,rz,ppm} (position vector convention,
 * like TOWGS84), {@code molodensky:dx,dy,dz,src_a,src_b}, {@code geocentric} (ellipsoid change only),
 * {@code identity} or {@code other:<transform classes>}. Each step is prefixed with the semi-major axis GeoTools
 * uses for the source ellipsoid ({@code a=...}).
 *
 * <p>
 * Used by the {@code generate_epsg_table} test in {@code crates/planetiler-core/src/geo/crs/epsg.rs} to build and
 * verify the embedded EPSG table: {@code tools/crs/java.sh DumpEpsg target/crsgen/epsg.tsv}.
 */
public class DumpEpsg {
  static String axes(CoordinateReferenceSystem crs) {
    var cs = crs.getCoordinateSystem();
    StringBuilder sb = new StringBuilder();
    for (int i = 0; i < cs.getDimension(); i++) {
      if (i > 0) {
        sb.append(',');
      }
      sb.append(cs.getAxis(i).getDirection().name());
    }
    return sb.toString();
  }

  static String clean(Object o) {
    return String.valueOf(o).replace('\t', ' ').replace('\n', ' ');
  }

  static void flatten(MathTransform t, List<MathTransform> out) {
    if (t instanceof ConcatenatedTransform c) {
      flatten(c.transform1, out);
      flatten(c.transform2, out);
    } else {
      out.add(t);
    }
  }

  static double param(AbstractMathTransform t, String name) {
    for (var v : t.getParameterValues().values()) {
      if (v instanceof ParameterValue<?> p && p.getDescriptor().getName().getCode().equals(name)) {
        return p.doubleValue();
      }
    }
    return 0;
  }

  /** Describes the datum shift from {@code base} to WGS84. */
  static String datum(CoordinateReferenceSystem base, CoordinateReferenceSystem wgs) throws Exception {
    var steps = new ArrayList<MathTransform>();
    flatten(CRS.findMathTransform(base, wgs, true), steps);
    String srcA = "";
    for (var step : steps) {
      String cls = step.getClass().getName();
      if (cls.contains("GeocentricTransform") && !cls.contains("Inverse") && srcA.isEmpty()) {
        srcA = "a=" + param((AbstractMathTransform) step, "semi_major") + ",";
      }
    }
    for (var step : steps) {
      String cls = step.getClass().getName();
      if (cls.endsWith("GeocentricTranslation")) {
        var t = (AbstractMathTransform) step;
        String method = t.getParameterValues().getDescriptor().getName().getCode();
        double sign = method.contains("Coordinate Frame") || method.contains("Coordinate frame") ? -1 : 1;
        return srcA + "helmert:" + param(t, "dx") + "," + param(t, "dy") + "," + param(t, "dz") + "," +
          sign * param(t, "ex") + "," + sign * param(t, "ey") + "," + sign * param(t, "ez") + "," + param(t, "ppm");
      }
      if (step instanceof LinearTransform linear && step.getSourceDimensions() == 3 &&
        step.getTargetDimensions() == 3) {
        // a Helmert transformation merged into a geocentric affine transform:
        // X' = T + (1 + s) R X with R = [[1, -rz, ry], [rz, 1, -rx], [-ry, rx, 1]] (position vector)
        var m = linear.getMatrix();
        double sec = 180 * 3600 / Math.PI;
        double s = m.getElement(0, 0) - 1;
        return srcA + "helmert:" + m.getElement(0, 3) + "," + m.getElement(1, 3) + "," + m.getElement(2, 3) + "," +
          m.getElement(2, 1) / (1 + s) * sec + "," + m.getElement(0, 2) / (1 + s) * sec + "," +
          m.getElement(1, 0) / (1 + s) * sec + "," + s * 1e6;
      }
      if (cls.contains("MolodenskyTransform") || cls.contains("Molodenski")) {
        var t = (AbstractMathTransform) step;
        return "molodensky:" + param(t, "dx") + "," + param(t, "dy") + "," + param(t, "dz") + "," +
          param(t, "src_semi_major") + "," + param(t, "src_semi_minor") + "," +
          (cls.contains("Abridged") || t.getParameterValues().getDescriptor().getName().getCode().contains("Abridged"));
      }
    }
    boolean geocentric = steps.stream().anyMatch(s -> s.getClass().getName().contains("GeocentricTransform"));
    if (geocentric) {
      return srcA + "geocentric";
    }
    boolean onlyAffine = steps.stream().allMatch(s -> s.getClass().getName().contains("Affine") ||
      s.getClass().getName().contains("LinearTransform") || s.getClass().getName().contains("IdentityTransform"));
    if (onlyAffine) {
      return "identity";
    }
    StringBuilder sb = new StringBuilder("other:");
    for (var s : steps) {
      sb.append(s.getClass().getSimpleName()).append(' ');
    }
    return sb.toString();
  }

  public static void main(String[] args) throws Exception {
    var out = new PrintStream(new FileOutputStream(args[0]), false, "UTF-8");
    var codes = new TreeSet<Integer>();
    for (String c : CRS.getSupportedCodes("EPSG")) {
      try {
        codes.add(Integer.parseInt(c.replaceAll("^EPSG:", "")));
      } catch (Exception e) {
        // not numeric
      }
    }
    System.err.println("codes: " + codes.size());
    var wgs = CRS.decode("EPSG:4326", true);
    var opf = CRS.getCoordinateOperationFactory(true);
    // non-strict WKT formatting (Formattable.toWKT(Citation, int, boolean) is private) so that CRS with axes like
    // "South along 45 deg East" can be formatted too
    var toWkt = Formattable.class.getDeclaredMethod("toWKT", Citation.class, int.class, boolean.class);
    toWkt.setAccessible(true);
    for (int code : codes) {
      String id = "EPSG:" + code;
      try {
        CoordinateReferenceSystem nat;
        try {
          nat = CRS.decode(id, false);
        } catch (Exception e) {
          out.println(code + "\tERR\t" + clean(e));
          continue;
        }
        String kind = nat instanceof GeographicCRS ? "G" : nat instanceof ProjectedCRS ? "P" : null;
        if (kind == null || nat.getCoordinateSystem().getDimension() != 2) {
          continue;
        }
        String order = CRS.getAxisOrder(nat).name();
        var lf = CRS.decode(id, true);
        String wkt;
        try {
          wkt = ((String) toWkt.invoke(lf, Citations.OGC, 0, false)).replaceAll("\\s*\\n\\s*", "");
        } catch (Exception e) {
          wkt = "UNFORMATTABLE";
        }
        String datum;
        try {
          datum = datum(lf instanceof ProjectedCRS p ? p.getBaseCRS() : lf, wgs);
        } catch (Exception e) {
          datum = "ERR " + clean(e);
        }
        String sample;
        String opName;
        try {
          var bbox = CRS.getGeographicBoundingBox(nat);
          double[][] pts = {};
          if (bbox != null) {
            double w = bbox.getWestBoundLongitude(), e = bbox.getEastBoundLongitude();
            double s = bbox.getSouthBoundLatitude(), n = bbox.getNorthBoundLatitude();
            if (e < w) {
              e += 360;
            }
            pts = new double[][]{
              {(w + e) / 2, (s + n) / 2},
              {w * 0.75 + e * 0.25, s * 0.25 + n * 0.75},
            };
          }
          var fwd = CRS.findMathTransform(wgs, lf, true);
          var inv = CRS.findMathTransform(lf, wgs, true);
          try {
            opName = opf.createOperation(lf, wgs).getName().getCode();
          } catch (Exception ex) {
            opName = "?";
          }
          StringBuilder sb = new StringBuilder();
          for (double[] p : pts) {
            if (p[0] > 180) {
              p[0] -= 360;
            }
            double[] xy = new double[2];
            fwd.transform(p, 0, xy, 0, 1);
            double[] ll = new double[2];
            inv.transform(xy, 0, ll, 0, 1);
            if (!sb.isEmpty()) {
              sb.append(';');
            }
            sb.append(p[0]).append(',').append(p[1]).append(',').append(xy[0]).append(',').append(xy[1]).append(',')
              .append(ll[0]).append(',').append(ll[1]);
          }
          sample = sb.toString();
        } catch (Exception ex) {
          sample = "ERR " + clean(ex);
          opName = "?";
        }
        out.println(code + "\t" + kind + "\t" + order + "\t" + axes(nat) + "\t" + clean(opName) + "\t" + sample
          + "\t" + wkt + "\t" + clean(datum));
      } catch (Exception e) {
        out.println(code + "\tERR\t" + clean(e));
      }
    }
    out.close();
  }
}
