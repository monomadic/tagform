#!/usr/bin/env -S swift -suppress-warnings

// geocode QUERY
// geocode --reverse LATITUDE LONGITUDE
//
// tagform's place lookup (DESIGN §5.5), asked through MapKit -- the service
// behind Finder's "Created in Makati, Philippines" line and behind
// rename-footage's reverse-geocode, so a place typed here, a clip named from
// its coordinates and what macOS says about the same file all agree.
//
// A forward lookup is a local search, not an address parse: "Coro Hotel
// Makati" has to find the hotel, and only a point-of-interest search does.
// A reverse lookup answers the coordinates a camera stored.
//
// Prints one line per hit, tab-separated, best first:
//
//     NAME  CITY  STATE  COUNTRY  LATITUDE  LONGITUDE
//
// A part the geocoder has nothing for is left empty so the columns stay
// aligned. Exits 1 with nothing on stdout for no hits, 2 for a bad call, and
// says why on stderr, so a caller can carry on without a place rather than
// abort. Needs the network; needs no location permission, since it asks
// about a place or a coordinate the caller already holds, never about where
// this Mac is.
//
// MKMapItem.placemark is deprecated in macOS 26 in favour of .address, but
// the replacement only vends preformatted strings ("Makati, Metro Manila"),
// while these parts are written into separate IPTC tags. Hence the placemark,
// and hence -suppress-warnings: the script is compiled on each use, and
// without it every run would print the deprecation to stderr.

import Foundation
import MapKit

let args = Array(CommandLine.arguments.dropFirst())

func out(_ s: String) { FileHandle.standardOutput.write((s + "\n").data(using: .utf8)!) }
func fail(_ m: String) { FileHandle.standardError.write(("geocode: " + m + "\n").data(using: .utf8)!) }
// MapKit reports "nothing matched" as an error; a caller wants it as no hits.
func failed(_ error: Error) {
    if (error as? MKError)?.code == .placemarkNotFound { fail("no hits") } else { fail(error.localizedDescription) }
}
func col(_ s: String?) -> String { (s ?? "").replacingOccurrences(of: "\t", with: " ") }

var done = false
var status: Int32 = 1

func emit(_ items: [MKMapItem]) {
    for it in items {
        let p = it.placemark
        let c = p.coordinate
        out([col(it.name), col(p.locality), col(p.administrativeArea), col(p.country),
             String(format: "%.5f", c.latitude), String(format: "%.5f", c.longitude)]
            .joined(separator: "\t"))
    }
    if items.isEmpty { fail("no hits") }
    status = items.isEmpty ? 1 : 0
}

Task {
    defer { done = true }
    if args.first == "--reverse" {
        guard args.count == 3, let lat = Double(args[1]), let lon = Double(args[2]) else {
            fail("usage: geocode --reverse LATITUDE LONGITUDE"); status = 2; return
        }
        let where_ = CLLocationCoordinate2D(latitude: lat, longitude: lon)
        guard CLLocationCoordinate2DIsValid(where_) else {
            fail("coordinates out of range: \(lat), \(lon)"); status = 2; return
        }
        guard let req = MKReverseGeocodingRequest(location: CLLocation(latitude: lat, longitude: lon)) else {
            fail("could not build a request for \(lat), \(lon)"); return
        }
        do { emit(try await req.mapItems) } catch { failed(error) }
        return
    }
    let query = args.joined(separator: " ").trimmingCharacters(in: .whitespaces)
    guard !query.isEmpty else {
        fail("usage: geocode QUERY | geocode --reverse LATITUDE LONGITUDE"); status = 2; return
    }
    let r = MKLocalSearch.Request()
    r.naturalLanguageQuery = query
    r.resultTypes = [.pointOfInterest, .address]
    do { emit(try await MKLocalSearch(request: r).start().mapItems) } catch { failed(error) }
}

while !done { RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.05)) }
exit(status)
